//! Embedded icon assets for the browser build.
//!
//! GPUI renders `IconName`s through an `AssetSource`; on native the desktop
//! app registers `gpui_kit::assets::Assets` (which embeds this same default
//! icon set on disk-backed builds), while the wasm variant of that crate only
//! offers an on-demand CDN fetch. To keep the payload self-contained (the
//! daemon may run offline), `build.rs` embeds the vendored SVGs from
//! `assets/icons/` — the same default component set gpui-kit ships natively.

use gpui_kit::gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

include!(concat!(env!("OUT_DIR"), "/icons.rs"));

/// Serves the embedded component icons by their GPUI asset paths.
pub struct IconAssets;

impl AssetSource for IconAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(load_embedded_icon(path))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        // The SVG renderer only consults `load`; nothing in this app
        // enumerates assets.
        Ok(Vec::new())
    }
}
