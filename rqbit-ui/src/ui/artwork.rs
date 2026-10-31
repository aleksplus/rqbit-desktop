//! Artwork loading, masking, and tinting: turns decorated [`TorrentArtwork`]
//! references into masked [`RenderImage`]s for the detail panel's Overview
//! corner and the torrent list rows.
//!
//! GPUI can only clip images to rectangles (plus corner radii), so the
//! poster's triangle shape and its transparent diagonal gradient are baked
//! into the bitmap's alpha channel here: bytes come from the host
//! ([`UiHost::fetch_bytes`]), are decoded with the `image` crate,
//! center-cropped to the target size (cover), and multiplied with a
//! per-pixel triangle mask. The result is cached per (artwork key, shape,
//! size, tint) in a bounded LRU; loads run detached through `App::spawn`, so
//! callers just re-render on their next poll and pick the image up.
//!
//! A torrent can carry several artwork *candidates* (best source first —
//! hook poster, MyShows poster, media preview): the first candidate whose
//! image is already loaded wins, and every not-yet-loaded candidate gets a
//! background load kicked, so when the primary's bytes fail (a temporarily
//! broken poster URL), the next candidate's image simply appears on the
//! next poll.
//!
//! When the host asks for a theme tint ([`UiHost::artwork_tint`]), each
//! image is additionally recolored as a duotone from the theme's background
//! to its primary color ([`apply_tint`]) — the poster follows the active
//! theme instead of carrying its own colors.
//!
//! The mask (see [`triangle_alpha`]): a right triangle flush with the
//! element's top-right corner — sharp angle at the top-left vertex, 90° at
//! the top-right, sharp angle at the bottom-right — whose diagonal edge
//! fades to fully transparent (a soft blend into the panel background) while
//! the top and right edges stay crisp.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, Global, RenderImage};
use gpui_kit::component::ActiveTheme as _;
use image::imageops::FilterType;
use image::{DynamicImage, Frame, ImageBuffer, Rgba};
use rqbit_protocol::types::TorrentArtwork;

use crate::host::UiHost;

/// Rendered (logical) size of the Overview tab's corner artwork.
pub const DETAIL_ARTWORK_SIZE: f32 = 256.0;
/// Rendered (logical) width of a torrent row's corner wedge; its height
/// follows the table row.
pub const ROW_WEDGE_WIDTH: f32 = 64.0;
/// Rendered (logical) height of a torrent row's corner wedge — gpui-kit's
/// default (`Size::Medium`) table row height.
pub const ROW_WEDGE_HEIGHT: f32 = 32.0;

/// How far along the perpendicular from the diagonal edge the fade runs, as
/// a fraction of the triangle's full depth (1.0 = the whole shape fades).
const FADE_FRACTION: f32 = 0.85;

/// Debug escape hatch: set `RQBIT_ARTWORK_OPAQUE=1` to render artwork as a
/// plain opaque cover-crop (no triangle, no gradient fade) — tells "the
/// image loads but the mask hides it" apart from "the image never loads".
fn mask_disabled() -> bool {
    static OPAQUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OPAQUE.get_or_init(|| {
        std::env::var("RQBIT_ARTWORK_OPAQUE")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    })
}

/// The two endpoints of the theme-tint duotone: an image pixel's luminance
/// maps from `shadow` (black) to `highlight` (white). Built from the active
/// theme so posters follow it — with the default palettes that is a
/// light-mono wash in dark mode and a dark-mono one in light mode; a custom
/// colored theme tints posters with its own hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtworkTint {
    shadow: Rgba<u8>,
    highlight: Rgba<u8>,
}

impl ArtworkTint {
    /// The active theme's duotone endpoints: background (shadows) to
    /// primary (highlights).
    pub fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            shadow: to_u8(theme.background),
            highlight: to_u8(theme.primary),
        }
    }

    /// A stable id for the cache key: the packed endpoint colors, so a
    /// theme switch (different colors) re-renders tinted images.
    fn id(&self) -> u64 {
        (pack(self.shadow) as u64) << 32 | pack(self.highlight) as u64
    }
}

/// gpui colors are f32 HSLA; the mask/tint math runs on 8-bit RGBA.
fn to_u8(color: gpui::Hsla) -> Rgba<u8> {
    let rgba = gpui::Rgba::from(color);
    Rgba([
        (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.a * 255.0).round().clamp(0.0, 255.0) as u8,
    ])
}

fn pack(color: Rgba<u8>) -> u32 {
    u32::from_be_bytes(color.0)
}

/// Recolor one pixel as a duotone: its Rec. 709 luminance picks a point on
/// the shadow→highlight ramp. Alpha is preserved, so this composes with the
/// triangle mask either way round.
pub fn tint_pixel([r, g, b, a]: [u8; 4], tint: &ArtworkTint) -> [u8; 4] {
    let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    let mix =
        |s: u8, h: u8| (f32::from(s) + (f32::from(h) - f32::from(s)) * luma / 255.0).round() as u8;
    [
        mix(tint.shadow.0[0], tint.highlight.0[0]),
        mix(tint.shadow.0[1], tint.highlight.0[1]),
        mix(tint.shadow.0[2], tint.highlight.0[2]),
        a,
    ]
}

/// Which masked rendition of an artwork to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtworkShape {
    /// Overview tab corner: square, triangle mask + diagonal fade.
    Detail,
    /// Torrent list row wedge: the same mask at the row's aspect.
    Row,
    /// Plain rectangle (hook Image-kind cells); no mask.
    Rect,
}

impl ArtworkShape {
    /// The bitmap size the image is decoded/cropped/masked at: 2× the
    /// logical render size for crispness on hidpi, but a fixed square for
    /// the (square) detail corner so the mask aspect stays stable.
    fn bake_size(self, render_w: f32, render_h: f32) -> (u32, u32) {
        match self {
            ArtworkShape::Detail => (512, 512),
            _ => (
                ((render_w * 2.0).round() as u32).max(1),
                ((render_h * 2.0).round() as u32).max(1),
            ),
        }
    }
}

/// Alpha (0..1) of the triangle mask at one pixel: 0 outside the triangle,
/// inside it a smoothstep ramp from the diagonal edge (transparent) toward
/// the top-right corner (opaque). Triangle A(0,0) – B(w,0) – C(w,h).
pub fn triangle_alpha(w: u32, h: u32, x: f32, y: f32) -> f32 {
    let w = w as f32;
    let h = h as f32;
    // Hypotenuse through A and C; the normal (h, -w) points inside.
    let side = h * x - w * y;
    if side <= 0.0 {
        return 0.0;
    }
    let norm = (w * w + h * h).sqrt();
    let dist = side / norm;
    let max_dist = w * h / norm; // depth at corner B
    let fade = (FADE_FRACTION * max_dist).max(1.0);
    let t = (dist / fade).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Scale the image so it covers the target box, then center-crop to it.
fn cover_crop(img: DynamicImage, w: u32, h: u32) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let scale = (w as f32 / iw as f32).max(h as f32 / ih as f32);
    let sw = ((iw as f32 * scale).round() as u32).max(w).max(1);
    let sh = ((ih as f32 * scale).round() as u32).max(h).max(1);
    let scaled = img.resize_exact(sw, sh, FilterType::Triangle);
    let x = (sw - w) / 2;
    let y = (sh - h) / 2;
    scaled.crop_imm(x, y, w, h).to_rgba8()
}

fn apply_shape_mask(buf: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, shape: ArtworkShape) {
    if shape == ArtworkShape::Rect {
        return;
    }
    let (w, h) = buf.dimensions();
    for (x, y, pixel) in buf.enumerate_pixels_mut() {
        let [r, g, b, a] = pixel.0;
        let alpha = triangle_alpha(w, h, x as f32 + 0.5, y as f32 + 0.5);
        let masked = (f32::from(a) / 255.0 * alpha * 255.0).round() as u8;
        *pixel = Rgba([r, g, b, masked]);
    }
}

/// Decode, center-crop, tint, mask, and wrap into a [`RenderImage`]. Runs
/// on the background executor (a 512² JPEG costs a few milliseconds).
///
/// The frame buffer is left in **BGRA byte order**: gpui-pre's own decoders
/// (`decode_static_image_from_decoder`, the GIF/WebP paths) all convert
/// `into_rgba8()` and then swap bytes 0↔2 before building frames — the
/// renderer reads the atlas as BGRA, so feeding plain RGBA renders every
/// poster with red and blue exchanged (the blue-tint look).
fn build_image(
    bytes: &[u8],
    shape: ArtworkShape,
    render_w: f32,
    render_h: f32,
    tint: Option<ArtworkTint>,
) -> Result<Arc<RenderImage>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (bw, bh) = shape.bake_size(render_w, render_h);
    let mut buf = cover_crop(img, bw, bh);
    if let Some(tint) = &tint {
        for pixel in buf.pixels_mut() {
            *pixel = Rgba(tint_pixel(pixel.0, tint));
        }
    }
    if !mask_disabled() {
        apply_shape_mask(&mut buf, shape);
    }
    for pixel in buf.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buf)])))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    key: String,
    shape: ArtworkShape,
    w: u32,
    h: u32,
    /// Packed duotone endpoints (0 = untinted): a theme switch re-renders.
    tint: u64,
}

#[derive(Default)]
struct CacheEntry {
    image: Option<Arc<RenderImage>>,
    /// When the last load for this key failed. A failure is retried after
    /// [`FAILED_RETRY`] so a transient hiccup (server busy, brief 404 while
    /// a preview extracts) can't suppress an image until the key changes.
    failed_at: Option<Instant>,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    /// LRU order, least-recently-used first. Only successful entries count.
    order: Vec<CacheKey>,
    inflight: HashSet<CacheKey>,
}

impl CacheState {
    fn touch(&mut self, key: &CacheKey) {
        if let Some(ix) = self.order.iter().position(|k| k == key) {
            let entry = self.order.remove(ix);
            self.order.push(entry);
        }
    }

    fn evict(&mut self) {
        const MAX_ENTRIES: usize = 256;
        while self.order.len() > MAX_ENTRIES {
            let evicted = self.order.remove(0);
            self.entries.remove(&evicted);
        }
    }
}

/// How long a failed artwork load waits before the next render retries it.
const FAILED_RETRY: Duration = Duration::from_secs(30);

/// The process-wide masked-image cache. The `Mutex` is only ever held for
/// brief map operations, never across an await.
#[derive(Default)]
struct GlobalArtworkCache(Mutex<CacheState>);

impl Global for GlobalArtworkCache {}

/// Look up the best loaded artwork image among the candidates, starting
/// background loads for every candidate that isn't cached yet. Returns
/// `None` while loading or after a failure; callers re-render on their
/// regular poll cycle, so a completed load appears without any further
/// plumbing. Candidates come in priority order — the first *loaded* one
/// wins, and with a failed primary the next candidate's image takes over.
pub fn masked_artwork(
    host: &Arc<dyn UiHost>,
    candidates: &[TorrentArtwork],
    shape: ArtworkShape,
    render_w: f32,
    render_h: f32,
    cx: &mut App,
) -> Option<Arc<RenderImage>> {
    let tint = host.artwork_tint().then(|| ArtworkTint::from_theme(cx));
    let tint_id = tint.as_ref().map_or(0, ArtworkTint::id);
    if !cx.has_global::<GlobalArtworkCache>() {
        cx.set_global(GlobalArtworkCache::default());
    }

    for candidate in candidates {
        let cache_key = CacheKey {
            key: candidate.key.clone(),
            shape,
            w: render_w as u32,
            h: render_h as u32,
            tint: tint_id,
        };
        // A ready image wins immediately; a missing one gets a background
        // load kicked (throttled by the inflight set and the failure
        // cooldown) and the next candidate gets its chance.
        let loaded = {
            let Ok(mut state) = cx.global::<GlobalArtworkCache>().0.lock() else {
                return None;
            };
            let image = state.entries.get(&cache_key).and_then(|e| e.image.clone());
            if image.is_some() {
                state.touch(&cache_key);
            }
            image
        };
        if let Some(image) = loaded {
            return Some(image);
        }
        load_artwork_if_needed(
            host, candidate, cache_key, shape, render_w, render_h, tint, cx,
        );
    }
    None
}

/// Kick a background load for one candidate unless it is already loading or
/// cooling down after a failure. The caller has checked the cache; a cached
/// candidate never reaches this.
#[allow(clippy::too_many_arguments)]
fn load_artwork_if_needed(
    host: &Arc<dyn UiHost>,
    candidate: &TorrentArtwork,
    cache_key: CacheKey,
    shape: ArtworkShape,
    render_w: f32,
    render_h: f32,
    tint: Option<ArtworkTint>,
    cx: &mut App,
) {
    let (cooling, inflight) = {
        let Ok(state) = cx.global::<GlobalArtworkCache>().0.lock() else {
            return;
        };
        let cooling = state
            .entries
            .get(&cache_key)
            .and_then(|e| e.failed_at)
            .is_some_and(|at| at.elapsed() < FAILED_RETRY);
        (cooling, state.inflight.contains(&cache_key))
    };
    if cooling || inflight {
        return;
    }

    let url = match host.artwork_url(&candidate.path) {
        Some(url) => url,
        None => return,
    };
    if let Ok(mut state) = cx.global::<GlobalArtworkCache>().0.lock() {
        state.inflight.insert(cache_key.clone());
    }

    let host = host.clone();
    cx.spawn(async move |cx| {
        let build = async {
            let bytes = host.fetch_bytes(url.clone()).await?;
            cx.background_executor()
                .spawn(async move { build_image(&bytes, shape, render_w, render_h, tint) })
                .await
        }
        .await;
        cx.update(|cx| {
            let Ok(mut state) = cx.global::<GlobalArtworkCache>().0.lock() else {
                return;
            };
            state.inflight.remove(&cache_key);
            let entry = state.entries.entry(cache_key.clone()).or_default();
            match build {
                Ok(image) => {
                    entry.image = Some(image);
                    entry.failed_at = None;
                    state.order.push(cache_key);
                    state.evict();
                }
                Err(_) => {
                    entry.failed_at = Some(Instant::now());
                }
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_alpha_outside_is_transparent() {
        let (w, h) = (512u32, 512u32);
        // Below the hypotenuse (bottom-left half).
        assert_eq!(
            triangle_alpha(w, h, 10.0, w as f32 * 10.0 / h as f32 + 1.0),
            0.0
        );
        // Bottom-left corner: far outside.
        assert_eq!(triangle_alpha(w, h, 0.0, h as f32), 0.0);
    }

    #[test]
    fn triangle_alpha_fades_from_hypotenuse_to_corner() {
        let (w, h) = (512u32, 512u32);
        // On the hypotenuse: transparent.
        assert_eq!(triangle_alpha(w, h, 0.0, 0.0), 0.0);
        // Corner B (top-right): fully opaque.
        assert_eq!(triangle_alpha(w, h, w as f32 - 1.0, 0.5), 1.0);
        // Center of the shape: somewhere in between.
        let mid = triangle_alpha(w, h, w as f32 * 0.75, h as f32 * 0.25);
        assert!(mid > 0.0 && mid < 1.0, "mid alpha {mid}");
        // Monotonic: moving toward the corner raises alpha.
        let closer = triangle_alpha(w, h, w as f32 * 0.9, h as f32 * 0.1);
        assert!(closer > mid);
    }

    #[test]
    fn cover_crop_keeps_target_size() {
        let img = DynamicImage::new_rgba8(100, 400);
        let buf = cover_crop(img, 512, 512);
        assert_eq!(buf.dimensions(), (512, 512));
    }

    /// The renderer reads `RenderImage` frames as BGRA (gpui-pre's own
    /// decoders swap `into_rgba8()` bytes 0↔2 before building frames);
    /// feeding plain RGBA rendered every poster with red and blue
    /// exchanged — the blue-tint regression.
    #[test]
    fn build_image_produces_bgra_bytes() {
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([255, 0, 0, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let img = build_image(&png, ArtworkShape::Rect, 1.0, 1.0, None).unwrap();
        let bytes = img.as_bytes(0).unwrap();
        assert_eq!(bytes[0], 0, "blue byte must be 0 for a red input (BGRA)");
        assert_eq!(bytes[2], 255, "red input must land in the blue byte slot");
        assert_eq!(bytes[3], 255, "alpha is untouched by the swap");
    }

    /// The tint maps a grayscale pixel onto the shadow→highlight ramp:
    /// black lands on the shadow color, white on the highlight, mid-gray
    /// halfway; alpha is preserved.
    #[test]
    fn tint_pixel_maps_luma_onto_ramp() {
        let tint = ArtworkTint {
            shadow: Rgba([0, 0, 0, 255]),
            highlight: Rgba([100, 200, 50, 255]),
        };
        assert_eq!(tint_pixel([0, 0, 0, 255], &tint), [0, 0, 0, 255]);
        assert_eq!(tint_pixel([255, 255, 255, 255], &tint), [100, 200, 50, 255]);
        let [r, g, b, a] = tint_pixel([128, 128, 128, 200], &tint);
        assert_eq!((r, g, b, a), (50, 100, 25, 200));
        // A gray poster keeps its shading: brighter pixels sit closer to
        // the highlight on every channel.
        let dark = tint_pixel([40, 40, 40, 255], &tint);
        let bright = tint_pixel([200, 200, 200, 255], &tint);
        assert!(dark[1] < bright[1]);
    }

    /// The tint passes through `build_image` (and lands BGRA-swapped like
    /// everything else), keyed so a different theme re-renders.
    #[test]
    fn build_image_applies_tint() {
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(ImageBuffer::from_pixel(1, 1, Rgba([255, 255, 255, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let tint = ArtworkTint {
            shadow: Rgba([10, 20, 30, 255]),
            highlight: Rgba([40, 60, 80, 255]),
        };
        let img = build_image(&png, ArtworkShape::Rect, 1.0, 1.0, Some(tint)).unwrap();
        let bytes = img.as_bytes(0).unwrap();
        // A white input lands exactly on the highlight color (BGRA: bytes
        // [0] and [2] are swapped blue/red).
        assert_eq!((bytes[0], bytes[1], bytes[2]), (80, 60, 40));
        // Different endpoints → different cache id.
        let other = ArtworkTint {
            shadow: Rgba([10, 20, 30, 255]),
            highlight: Rgba([41, 60, 80, 255]),
        };
        assert_ne!(tint.id(), other.id());
        assert_eq!(tint.id(), tint.id());
    }
}
