//! Browser (wasm32-unknown-unknown) build of the rqbit GPUI client.
//!
//! Reuses the shared views from `rqbit-ui` and talks to a headless
//! `app-daemon` over the `rqbit-protocol` WebSocket. Build with the
//! documented pipeline (see the Makefile `web-client` targets):
//! `wasm-bindgen --target web` plus a static file server serving this
//! crate's `index.html` and the generated `pkg/` payload.

#[cfg(target_family = "wasm")]
pub mod remote_client;

#[cfg(target_family = "wasm")]
mod file_picker;

#[cfg(target_family = "wasm")]
mod icon_assets;

#[cfg(target_family = "wasm")]
mod web_host;

#[cfg(target_family = "wasm")]
mod entry;

#[cfg(target_family = "wasm")]
pub use entry::run;

/// Non-wasm stub: this crate only produces code for the browser target.
#[cfg(not(target_family = "wasm"))]
pub fn unsupported_on_native() {}
