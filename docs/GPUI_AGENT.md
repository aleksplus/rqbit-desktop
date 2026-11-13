# GPUI Agent UI Testing

`desktop-gpui` has an opt-in [`gpui-agent`](https://github.com/hexuria/gpui-agent)
bridge for local automated UI testing. It exposes semantic UI snapshots and
actions, in-process virtual mouse/keyboard input, global GPUI actions, and
macOS window screenshots. It is not CDP and does not use OS-level input.

The integration is pinned to upstream commit
`b0007f2904b56d0ba44e48bd301da43d7bd286f2` (protocol v2). The browser/WASM
client is not an agent host.

## Security model

- The `agent-testing` Cargo feature is **off by default**.
- The listener starts only when `GPUI_AGENT=1` (the other upstream truthy
  values also work).
- `GPUI_AGENT_TOKEN` must be non-empty, including on loopback. Clients use a
  per-connection HMAC challenge; the raw token is not sent on the wire.
- Only loopback addresses are accepted. rqbit rejects the upstream tokenless
  and remote-bind escape flags.
- Release builds additionally require `GPUI_AGENT_ALLOW_RELEASE=1`.
- There is no persistent settings toggle and no browser control plane.

An authorized client can read visible torrent names, drive the UI, and shut
down the app. With the real backend it can also perform the same torrent
actions as the exposed widgets. Use the fixture for destructive or repeatable
scripts. Do not put secrets in visible windows; screenshots and snapshots are
not redacted beyond the intentionally limited semantic contract.

## Isolated fixture

Set `RQBIT_AGENT_FIXTURE=1` to bypass `State::new` and use two deterministic
in-memory torrents, `Fixture Alpha` (live) and `Fixture Beta` (paused). Fixture
mode does not read the desktop config or start librqbit, DHT, peer listeners,
HTTP/Web View servers, UPnP, hooks, artwork workers, persistence, or native
file pickers. Pause/start/file-selection/add/forget/delete operations mutate
only fixture DTOs; delete never touches disk.

Build and run:

```bash
cargo run -p desktop-gpui --features agent-testing
```

with this environment:

```bash
export GPUI_AGENT=1
export RQBIT_AGENT_FIXTURE=1
export GPUI_AGENT_ADDR=127.0.0.1:17421
export GPUI_AGENT_TOKEN=dev-secret
export GPUI_AGENT_CONNECT_ID=rqbit-desktop-fixture
export GPUI_AGENT_SCREENSHOT_DIR="$PWD/artifacts/agent"
```

Use a unique token in real development environments. The example value is a
test fixture label, not a secret suitable for an exposed installation.

For a **real session**, omit `RQBIT_AGENT_FIXTURE`. Do not run the destructive
fixture recipe against real data.

## CLI and discovery

Install the CLI from the same reviewed revision (the package is
`gpui-agent-cli`; the executable is `gpui-agent`):

```bash
cargo install \
  --git https://github.com/hexuria/gpui-agent \
  --rev b0007f2904b56d0ba44e48bd301da43d7bd286f2 \
  gpui-agent-cli
```

Then, with the same address/token environment:

```bash
gpui-agent --connect rqbit-desktop-fixture wait
gpui-agent --connect last hello
gpui-agent --connect last snapshot --pretty
```

For a real development run, use `--connect desktop-gpui` or the explicit
`GPUI_AGENT_CONNECT_ID`. A packaged app can be found through bundle ID
`com.rqbit.gpui-desktop`. `--addr 127.0.0.1:17421` bypasses discovery.
`hello.protocol` is `2` and `hello.auth` is `required`.

## Stable semantic contract

The initial vertical slice exports:

- Window/pages: `rqbit-window`, `page-main`, `page-settings`, `page-logs`.
- Controls: `add-magnet`, `settings`, `logs`, `theme`, `start`, `pause`,
  `delete`, `refresh`, `explorer-toggle`.
- Inputs/list: `torrent-search`, `status-filter`, `torrent-list`.
- Rows: `torrent-{torrent_id}` and `torrent-{torrent_id}-status`.
- Details: `torrent-detail-{torrent_id}`,
  `detail-tab-{overview|trackers|peers|files}`, and
  `torrent-{torrent_id}-file-{file_index}`.
- Magnet dialog: `dialog-magnet`, `magnet-input`, `magnet-add`,
  `magnet-cancel`.
- Delete dialog: `dialog-delete`, `delete-files`, `delete-confirm`,
  `delete-cancel`.
- About dialog: `dialog-about`, `about-close`.

Rows and files use domain IDs, never visible row positions. Only filtered rows
are exported. Modal backgrounds and hidden pages are marked invisible.
Snapshots currently have no revision (`rev = null`), so every request rebuilds
the tree; content fingerprints still support `expect_fingerprint` stale-target
checks.

Semantic actions call the same `MainPanel`/detail methods as human widget
handlers. Unknown targets and unapproved `invoke` names fail closed. The only
custom invokes are `app.close_window` (close the GPUI window without stopping
the session) and `fixture.close_window` (the fixture-only equivalent used by
the smoke recipe). Settings values, log contents, hook configuration, arbitrary filesystem paths, shell
commands, and native file-picker automation are not exported.

Global actions are available through the keybinding operation:

- `app.open` — activate or recreate the main window, including from the tray.
- `app.about` — open the About dialog.
- `app.quit` — dangerous; requires `confirm=true`.

## Waiting, tray behavior, and shutdown

`wait` and `wait-until` use non-blocking GPUI timers, not upstream's blocking
sleep loop. Deadlines are capped at 30 seconds, matching the socket idle
timeout, and dispatch is budgeted per UI tick so a busy mailbox cannot starve
rendering. Use `wait-until` after operations that create a dialog/detail on a
later frame or perform asynchronous client work.

Closing the main window to the tray does **not** stop the mailbox driver:
`hello` and snapshots return `ready=false`, window actions return
`window_unavailable`, virtual input and screenshots report unavailable, and
bounded waits continue with deadlines. The agent never implicitly recreates a
window for an ordinary click or screenshot; use global `app.open` explicitly.
Agent Shutdown and confirmed `app.quit` use the common application quit path.
Native Quit, hold-to-quit, and close-without-tray also stop the listener.

## Virtual input and screenshots

Virtual click/type/key uses previous-frame painted bounds and GPUI
`Window::dispatch_event` / `dispatch_keystroke` on the UI thread. It never
warps the OS cursor or injects OS HID. Targets must be visible, enabled, and in
the application viewport. Non-ASCII text and unsupported keys retain upstream
errors.

On macOS, viewport screenshots use the current window's `NSWindow.windowNumber`
and upstream's fixed-argument `screencapture -l` helper. Grant the terminal/app
Screen Recording permission in macOS Privacy & Security settings. Output must
be a relative `.png` path under `GPUI_AGENT_SCREENSHOT_DIR`.

```bash
gpui-agent --connect last screenshot --out detail.png
```

Linux, Windows, headless, and windowless hosts return
`screenshot_unavailable`. Scrolled capture returns `scroll_unavailable` in this
pass. Production `gpui-kit/test-support` is not enabled for screenshots.

Finish a fixture run with:

```bash
gpui-agent --connect last shutdown
```

## Development checks

```bash
cargo check -p desktop-gpui --features agent-testing
cargo test -p rqbit-ui --features agent-testing
cargo test -p desktop-gpui --features agent-testing
```

The checked-in recipe is `docs/agent/rqbit-fixture-recipe.json`. After
installing the pinned CLI, run `scripts/agent-fixture-smoke.sh`. It builds and
launches a fresh fixture, validates the recipe and receipt, and cleans up the
process. Set `AGENT_SCREENSHOTS=1` to capture the flagged detail state on
macOS; GUI recipe and screenshot verification require a display.
Permission-dependent checks must not be reported as passed when skipped.
