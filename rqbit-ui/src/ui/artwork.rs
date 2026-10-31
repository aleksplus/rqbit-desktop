//! Artwork loading and masking: turns a decorated [`TorrentArtwork`]
//! reference into masked [`RenderImage`]s for the detail panel's Overview
//! corner and the torrent list rows.
//!
//! GPUI can only clip images to rectangles (plus corner radii), so the
//! poster's triangle shape and its transparent diagonal gradient are baked
//! into the bitmap's alpha channel here: bytes come from the host
//! ([`UiHost::fetch_bytes`]), are decoded with the `image` crate,
//! center-cropped to the target size (cover), and multiplied with a
//! per-pixel triangle mask. The result is cached per (artwork key, shape,
//! size) in a bounded LRU; loads run detached through `App::spawn`, so
//! callers just re-render on their next poll and pick the image up.
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

/// Decode, center-crop, mask, and wrap into a [`RenderImage`]. Runs on the
/// background executor (a 512² JPEG costs a few milliseconds).
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
) -> Result<Arc<RenderImage>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (bw, bh) = shape.bake_size(render_w, render_h);
    let mut buf = cover_crop(img, bw, bh);
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

/// Look up the masked artwork image, starting a background load when it is
/// not cached yet. Returns `None` while loading or after a failure; callers
/// re-render on their regular poll cycle, so a completed load appears
/// without any further plumbing.
pub fn masked_artwork(
    host: &Arc<dyn UiHost>,
    artwork: &TorrentArtwork,
    shape: ArtworkShape,
    render_w: f32,
    render_h: f32,
    cx: &mut App,
) -> Option<Arc<RenderImage>> {
    let cache_key = CacheKey {
        key: artwork.key.clone(),
        shape,
        w: render_w as u32,
        h: render_h as u32,
    };
    if !cx.has_global::<GlobalArtworkCache>() {
        cx.set_global(GlobalArtworkCache::default());
    }

    let (image, cooling, inflight) = {
        let Ok(mut state) = cx.global::<GlobalArtworkCache>().0.lock() else {
            return None;
        };
        let image = state.entries.get(&cache_key).and_then(|e| e.image.clone());
        if image.is_some() {
            state.touch(&cache_key);
        }
        // A failed load retries after the cooldown instead of staying
        // suppressed until the key changes.
        let cooling = state
            .entries
            .get(&cache_key)
            .and_then(|e| e.failed_at)
            .is_some_and(|at| at.elapsed() < FAILED_RETRY);
        (image, cooling, state.inflight.contains(&cache_key))
    };
    if let Some(image) = image {
        return Some(image);
    }
    if cooling || inflight {
        return None;
    }

    let url = host.artwork_url(&artwork.path)?;
    if let Ok(mut state) = cx.global::<GlobalArtworkCache>().0.lock() {
        state.inflight.insert(cache_key.clone());
    }

    let host = host.clone();
    cx.spawn(async move |cx| {
        let build = async {
            let bytes = host.fetch_bytes(url.clone()).await?;
            cx.background_executor()
                .spawn(async move { build_image(&bytes, shape, render_w, render_h) })
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
    None
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
        let img = build_image(&png, ArtworkShape::Rect, 1.0, 1.0).unwrap();
        let bytes = img.as_bytes(0).unwrap();
        assert_eq!(bytes[0], 0, "blue byte must be 0 for a red input (BGRA)");
        assert_eq!(bytes[2], 255, "red input must land in the blue byte slot");
        assert_eq!(bytes[3], 255, "alpha is untouched by the swap");
    }
}
