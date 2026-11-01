# CLAUDE.md

This file provides guidance for AI coding agents working in this repository.

## Project Overview

This is the **rqbit desktop client** repository: a native GPUI desktop app, which can additionally be used from a browser via a wasm build of the same UI. The desktop app is the primary product; the browser build is an alternative access mode served by the same code.

rqbit itself (`librqbit` and its sub-crates) is **not** part of this repo — it is used from crates.io, with local modifications applied as a cargo-patch-crate patch (see "rqbit dependency & patching" below).

## Workspace Crates

- `desktop-gpui` — the native desktop app (GPUI). Owns the librqbit session. Also bundles the daemon's HTTP/WS server, so the same session is reachable from a browser web view (settings → Web View, default http://127.0.0.1:3081).
- `rqbit-ui` — shared UI component library (librqbit-free, tokio-free).
- `rqbit-protocol` — wire protocol DTOs shared by UI and daemon (optional `librqbit` feature for `From<librqbit::…>` conversions).
- `app-daemon` — headless daemon exposing a session over the WebSocket protocol; the browser's backend. Bundled into desktop-gpui (one librqbit session serves both native UI and browser; it also embeds the built web client via `build.rs`).
- `web-gpui` — the same UI compiled to wasm (browser client), served by the daemon.

## Build Commands

```bash
# IMPORTANT: after cloning, materialize patched crate sources (see below)
cargo patch-crate

# Build (release)
cargo build --release

# Run the desktop app / headless daemon
cargo run -p desktop-gpui     # or: make desktop
cargo run -p app-daemon       # or: make daemon

# Build the wasm web client (nightly toolchain + wasm-bindgen-cli required;
# the wasm-bindgen-cli version must match the wasm-bindgen crate version in
# Cargo.lock — see Makefile for the one-time setup)
make web-client-build

# Run tests
cargo test                    # all workspace members
cargo test -p <crate>         # e.g. app-daemon has integration tests in app-daemon/tests/

# Lint
cargo fmt --all -- --check
cargo clippy --all-targets
```

Build/run notes:

- Only `cargo patch-crate` is mandatory setup. The web client payload is
  optional at build time: when `make web-client-build` hasn't run,
  `app-daemon/build.rs` embeds placeholders and the server serves a page
  telling you to build it.
- Tests open many file descriptors (torrents): on macOS raise the limit first,
  like CI does — `ulimit -n unlimited` before `cargo test`.

## Running

- `make desktop` — the desktop app: native UI plus a bundled web view of the
  SAME session at http://127.0.0.1:3081 (settings → Web View). HTTP API on
  `[::]:3030`.
- `make daemon` — headless daemon: web client + WS protocol at
  http://localhost:3080.
- Each process owns its own librqbit session (no shared/multi-instance
  sessions). Running both at once shows different torrent sets — use the
  desktop app's bundled web view when you want browser and native UI on the
  same torrents.
- Logs go to `/tmp/rqbit-log` when run via make (`RQBIT_LOG_FILE`, with debug
  verbosity incl. `librqbit=trace` — see the Makefile dev variables).

## rqbit dependency & patching (cargo-patch-crate)

`librqbit` comes from crates.io at the version pinned in the root `Cargo.toml`. Local API additions live in `patches/librqbit+<version>.patch`, applied by [cargo-patch-crate](https://github.com/mokeyish/cargo-patch-crate):

- `[workspace.metadata.patch]` in the root `Cargo.toml` lists patched crates (currently only `librqbit`; `librqbit-core` is a separate, unpatched dependency).
- `[patch.crates-io]` points them at `./target/patch/<crate>-<version>`.
- **`cargo patch-crate` must run once after cloning** (and after `cargo clean`, which wipes `target/patch/`) before any cargo build — it copies the pristine crate into `target/patch/` and applies `patches/*.patch`.

To modify librqbit: edit `target/patch/librqbit-<version>/` directly, then regenerate the patch file with `cargo patch-crate librqbit` and commit the updated `patches/librqbit+<version>.patch`.

To upgrade librqbit: bump the version in the root `Cargo.toml`, extract the new source (`cargo patch-crate`), re-apply the local API diff (the fork's librqbit history lives in this repo's git history under `crates/librqbit` — see `git log -- crates/librqbit`), then `cargo patch-crate librqbit` to regenerate the patch and fix any fallout.

## Rust Development

### Warnings & Linting
- After making code changes in Rust, always run `cargo check` and `cargo clippy`
  before declaring work complete. Never claim compilation success without verifying.
- When fixing compiler warnings, batch all related warnings together and fix them
  in a single pass. Run `cargo check 2>&1` to capture the full list before editing.

## General Rules
- Never declare a task complete until tests actually pass and compilation is verified.
  If compilation cannot be verified due to pre-existing errors, explicitly state that caveat.
- When asked to create a plan or design document, present the plan for user review
  BEFORE starting implementation. Do not begin coding until the user explicitly approves.
- Prefer typed errors over anyhow for verification paths. This keeps error handling
  consistent and allows callers to distinguish recoverable from hard errors.
- Always use `rg` instead of `grep` in shell commands.

## Protocol & Spec Compliance
- When working on BEP 52 / torrent-related code, always use v2 (BEP 52) structures
  and info dicts, not v1. The spec explicitly requires BEP 52 compliance.

## Other directives
- Code search: use semantic search (codeseeker mcp) to discover where a concept lives
  ("where is X handled", "how does Y work"); use rg to pin down exact strings and to
  verify results. rg is the safer default for exact terms — it never goes stale and
  never misses, while codeseeker's index can lag the working tree (it refreshes on
  query; force a sync after branch switches or large external changes).
- Use context7 for documentation.
- The UI is built on `gpui` (gpui-pre) + `gpui-kit`; the gpui-component docs at
  https://longbridge.github.io/gpui-component/docs/ are a useful API reference
  when building or modifying UI widgets.
- Feature specs live in `docs/` (e.g. `docs/HOOKS.md` documents the Hooks
  settings page, its config model and the outbound HTTP contract).
