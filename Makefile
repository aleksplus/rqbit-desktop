all:

# DEV variables
export RQBIT_LOG_FILE ?= /tmp/rqbit-log
export RQBIT_LOG_FILE_RUST_LOG ?= debug,librqbit=trace,upnp_serve=trace,librqbit_utp=debug
export RQBIT_UPNP_SERVER_ENABLE ?= true
export RQBIT_UPNP_SERVER_FRIENDLY_NAME ?= rqbit-dev
export RQBIT_HTTP_API_LISTEN_ADDR ?= [::]:3030
export RQBIT_ENABLE_PROMETHEUS_EXPORTER ?= true
export RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE ?= true
export RQBIT_HTTP_API_ALLOW_CREATE ?= true
export RQBIT_FASTRESUME = true

# NOTE: on LG TV using hostname is unstable for some reason, use IP address.

.PHONY: clean
clean:
	rm -rf target

# --- web client (browser build of the GPUI client) ---
WASM_TARGET ?= wasm32-unknown-unknown
# Must match the `wasm-bindgen` crate version in Cargo.lock.
WASM_BINDGEN_VERSION ?= 0.2.127

# The GPUI web stack needs a nightly toolchain: gpui-kit's wasm path pulls
# gpui-pre-web with its default `multithreaded` feature, whose wasm_thread
# dependency uses `#![feature(...)]` (there is no opt-out - both gpui-kit and
# gpui-pre-platform enable it unconditionally). This matches gpui-kit's own
# documented WebAssembly pipeline. RUSTC must be pinned too: cargo resolves
# rustc via PATH, which would pick Homebrew's rustc (no wasm std in its
# sysroot) even when cargo itself is rustup's.
WASM_TOOLCHAIN ?= nightly
WASM_CARGO ?= $(shell rustup which --toolchain $(WASM_TOOLCHAIN) cargo 2>/dev/null || command -v cargo)
WASM_RUSTC ?= $(shell rustup which --toolchain $(WASM_TOOLCHAIN) rustc 2>/dev/null)
WASM_BINDGEN ?= $(shell command -v wasm-bindgen 2>/dev/null || echo $(HOME)/.cargo/bin/wasm-bindgen)

# One-time setup (Homebrew rust has no rustup/wasm std by default):
#   brew install rustup && \
#   rustup toolchain install $(WASM_TOOLCHAIN) --profile minimal --target $(WASM_TARGET) && \
#   rustup run $(WASM_TOOLCHAIN) cargo install wasm-bindgen-cli --version $(WASM_BINDGEN_VERSION)

# Build the wasm payload. app-daemon embeds it at compile time, so after this
# run `make daemon` and open http://localhost:3080/ — the client is served by
# the daemon itself, and the WebSocket protocol lives at /ws on the same port.
.PHONY: web-client-build
web-client-build:
# Release by default: the debug payload is ~140 MB and slow to instantiate
# in the browser; release is ~29 MB. Pass DEBUG=1 for fast iteration builds.
	RUSTC=$(WASM_RUSTC) $(WASM_CARGO) build -p web-gpui --target $(WASM_TARGET) $(if $(DEBUG),,--release) && \
	$(WASM_BINDGEN) --target web \
		--out-dir web-gpui/dist/pkg \
		target/$(WASM_TARGET)/$(if $(DEBUG),debug,release)/web_gpui.wasm

# Headless rqbit daemon: serves the web client at http://localhost:3080/ and
# the WS protocol at /ws. Each process owns its own librqbit session — to see
# the same torrents in a browser and the desktop app, use the desktop app's
# bundled web view (default 127.0.0.1:3081) instead of running both.
.PHONY: daemon
daemon:
	cargo run -p app-daemon

# Desktop app: native UI plus a bundled web view of the SAME session at
# http://127.0.0.1:3081/ (settings → Web View). One librqbit instance serves
# both. Run `make web-client-build` once so the browser payload is embedded.
.PHONY: desktop
desktop:
	cargo run -p desktop-gpui
