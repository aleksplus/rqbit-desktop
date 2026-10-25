# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is the **rqbit desktop client** repository: a native GPUI desktop app (plus a browser build of the same UI) on top of the rqbit BitTorrent engine. rqbit itself (`librqbit` and its sub-crates) is **not** part of this repo — it is used from crates.io, with local modifications applied as a cargo-patch-crate patch (see "rqbit dependency & patching" below).

## Workspace Crates

- `desktop-gpui` — the native desktop app (GPUI). Owns the librqbit session.
- `app-daemon` — headless daemon exposing the session over a WebSocket protocol; bundled into desktop-gpui so one librqbit session serves both the native UI and the browser (it also embeds the built web client via `build.rs`).
- `rqbit-ui` — shared UI component library (librqbit-free, tokio-free).
- `rqbit-protocol` — wire protocol DTOs shared by UI and daemon (optional `librqbit` feature for `From<librqbit::…>` conversions).
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

# Build the wasm web client (nightly toolchain required, see Makefile)
make web-client-build

# Run tests
cargo test                    # all workspace members
cargo test -p rqbit-protocol <test_name>

# Lint
cargo fmt --all -- --check
cargo clippy --all-targets
```

## rqbit dependency & patching (cargo-patch-crate)

`librqbit = "=9.0.1"` comes from crates.io. Local API additions live in `patches/librqbit+9.0.1.patch`, applied by [cargo-patch-crate](https://github.com/mokeyish/cargo-patch-crate):

- `[workspace.metadata.patch]` in the root `Cargo.toml` lists patched crates.
- `[patch.crates-io]` points them at `./target/patch/<crate>-<version>`.
- **`cargo patch-crate` must run once after cloning** (and after `cargo clean`, which wipes `target/patch/`) before any cargo build — it copies the pristine crate into `target/patch/` and applies `patches/*.patch`.

To modify librqbit: edit `target/patch/librqbit-9.0.1/` directly, then regenerate the patch file with `cargo patch-crate librqbit` and commit `patches/librqbit+9.0.1.patch`.

To upgrade librqbit (e.g. to 9.0.2): bump the version in the root `Cargo.toml`, extract the new source (`cargo patch-crate`), re-apply the local API diff (the fork's librqbit history lives in this repo's git history under `crates/librqbit` — see `git log -- crates/librqbit`), then `cargo patch-crate librqbit` to regenerate the patch and fix any fallout.

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

## Protocol & Spec Compliance
- When working on BEP 52 / torrent-related code, always use v2 (BEP 52) structures
  and info dicts, not v1. The spec explicitly requires BEP 52 compliance.

## Other directives
- Use narsil mcp for code retrieval or search
- Use context7 for documentation
- Fetch https://longbridge.github.io/gpui-component/docs/ if needed.
- If you need to resort to running shell commands, always use "rg" instead of "grep".
