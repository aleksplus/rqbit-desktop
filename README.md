# rqbit-desktop - desktop app for rqbit

Native desktop app built with [GPUI](https://github.com/zed-industries/zed) (the UI framework behind Zed), and the same client compiled to WebAssembly running in the browser.


> **About this repository:** this is the rqbit **desktop client** repo (native GPUI app, browser build, headless daemon). The rqbit engine itself (`librqbit`) lives at [ikatson/rqbit](https://github.com/ikatson/rqbit) and on [crates.io](https://crates.io/crates/librqbit); this repo uses it as a Cargo dependency and applies its local API additions as a [cargo-patch-crate](https://github.com/mokeyish/cargo-patch-crate) patch (`patches/`).

The desktop app (`desktop-gpui`) is a native client built with GPUI. It bundles the daemon (`app-daemon`), so a single session serves both the native UI and a web view of the same client in the browser.

Download it in [Releases](https://github.com/aleksplus/rqbit/releases) for OSX, or run from source:

    make desktop

**After cloning, run `cargo install patch-crate && cargo patch-crate` once before building** — it materializes the patched librqbit source into `target/patch/`.

<img width="1000" alt="torrents list" src="https://github.com/user-attachments/assets/e80993d5-3837-48d2-9222-46f30515a188" />
<img width="1000" alt="torrent detail" src="https://github.com/user-attachments/assets/4f829963-ed90-471b-af75-78b0322f9c4b" />


## Web UI

Access at http://localhost:3081/. See screenshot below (torrent names and speeds are simulated).

<img width="1000" alt="Screenshot 2026-11-24 at 19 26 35 - blurred" src="https://github.com/user-attachments/assets/1c5ca6ce-f52a-48b9-91d4-4b1947841bc3" />

## Streaming support

rqbit can stream torrent files and smartly block the stream until the pieces are available. The pieces getting streamed are prioritized. All of this allows you to seek and live stream videos for example.

You can also stream to e.g. VLC or other players with HTTP URLs. Supports seeking too (through various range headers).
The streaming URLs look like http://IP:3030/torrents/<torrent_id>/stream/<file_id>

## Code organization

- desktop-gpui - native desktop app built with GPUI
- app-daemon - headless daemon exposing the rqbit session to UI clients over WebSocket; also serves the embedded web client
- rqbit-ui - shared UI components (librqbit-free, tokio-free)
- rqbit-protocol - wire protocol DTOs between the UI clients and the daemon
- web-gpui - browser (WebAssembly) build of the GPUI client
- the rqbit engine (`librqbit`: session, DHT, peer protocol, storage) is used from crates.io, with local API additions applied as a cargo-patch-crate patch in `patches/`
- [librqbit-utp](https://github.com/ikatson/librqbit-utp/) - uTP protocol
- [librqbit-dualstack-sockets](https://github.com/ikatson/librqbit-dualstack-sockets) - cross-platform IPv6+IPv4 listeners with canonical IPs
