# Hooks — Specification of the current implementation and API contract

Scope: the **Hooks** settings page built by `hook_groups`
(`desktop-gpui/src/ui/settings_page.rs:947`), the config model it edits, the
apply/persist lifecycle, and the outbound HTTP contract that the page's
description points to ("see the hooks module docs for the payload contract").
Everything below describes the code as it is today; nothing here is a proposal.

Source map:

| Concern | File |
|---|---|
| Settings UI (`hook_groups` + field helpers) | `desktop-gpui/src/ui/settings_page.rs:658-1007`, page wiring at `:1297-1304` |
| Config model (`HookConfig`, `HttpMethod`, `RqbitDesktopConfigHooks`) | `desktop-gpui/src/config.rs:17-61` |
| Event enum + module docs | `desktop-gpui/src/hooks/mod.rs` |
| JSON payloads / `file_selection` response | `desktop-gpui/src/hooks/payload.rs` |
| HTTP transport | `desktop-gpui/src/hooks/http.rs` |
| Dispatcher + completion watcher | `desktop-gpui/src/hooks/dispatcher.rs` |
| `torrent_columns` provider (poller, store, response parsing) | `app-daemon/src/columns.rs` (`ColumnsProvider`) |
| Custom-column wire types + push frame | `rqbit-protocol/src/types.rs` (`CustomColumnsState`), `messages.rs` (`PushUpdate::CustomColumns`) |
| Call sites | `state.rs:241`, `local_client.rs:47,203`, `folder_watcher.rs:203`, `desktop_host.rs:117` |

---

## 1. `hook_groups` — function contract

```rust
type ConfigArc = Arc<parking_lot::RwLock<RqbitDesktopConfig>>;   // settings_page.rs:21

fn hook_groups(config: ConfigArc) -> Vec<SettingGroup>           // settings_page.rs:947
```

* **Visibility**: private to `settings_page`. Its only caller is
  `SettingsPage::render`, which does
  `SettingPage::new("Hooks").description(…).groups(hook_groups(config.clone()))`
  (`settings_page.rs:1298-1304`). It therefore runs **on every render** of the
  settings page.
* **Input**: the page-local config handle (not the live session config — see §4).
  The function takes a single read lock to compute
  `count = config.read().hooks.hooks.len()` and releases it before building
  groups; every field closure re-acquires its own lock lazily.
* **Output**: `count + 1` groups, in this order:
  1. one `SettingGroup` per configured hook, index `ix ∈ 0..count`, in config
     order;
  2. a trailing group titled **"Manage"** containing only the **Add Hook** button.

  The result is never empty (the "Manage" group is always present).
* **Pure w.r.t. config**: building the groups does not mutate the config. All
  mutations happen later inside the field closures when the user interacts.
* **Index-based addressing**: every closure captures `(ConfigArc, ix)`. A hook is
  identified only by its position in `hooks.hooks`; there is no stable ID. The
  consequences are spelled out in §3.

### 1.1 Per-hook group

Title = the hook's `name`; if `name.trim()` is empty the title falls back to
`"Hook {ix + 1}"` (1-based).

Items, in fixed order:

| # | Label | Field kind | Reads | Writes | Layout / description |
|---|---|---|---|---|---|
| 1 | `Enabled` | `SettingField::switch` | `h.enabled` | `h.enabled = v` | horizontal; "Uncheck to pause this hook without deleting it." |
| 2 | `Name` | `SettingField::input` | `h.name` | `h.name = v` | vertical |
| 3 | `URL` | `SettingField::input` | `h.url` | `h.url = v` (no validation) | vertical; "e.g. http://127.0.0.1:8080/hook" |
| 4 | `Method` | `SettingField::dropdown` with options `("post","POST")`, `("get","GET")` | `h.method` → `"post"`/`"get"` | `"get"` → `Get`, anything else → `Post` | horizontal |
| 5 | `Timeout (seconds)` | `SettingField::input` | `h.timeout_secs.to_string()` | parse as `u64`; **invalid input is ignored**; valid input is clamped to `max(1)` | horizontal |
| 6 | `Events` | `SettingField::render` → row of 4 checkboxes | `h.events.contains(e)` | toggle: remove if present, else `push` | vertical; "Which lifecycle events trigger this hook." |
| 7 | `Headers (one per line, "Key: Value")` | `SettingField::render` → auto-growing textarea (1–6 rows) | `headers_to_text(h)` | `set_headers_text(h, text)` | vertical |
| 8 | `""` (empty label) | `SettingField::render` → outline button **Remove Hook** | — | `remove_hook(ix)` | vertical |

Field helper signatures (all in `settings_page.rs`):

```rust
fn hook_switch_field(config, ix, get: fn(&HookConfig)->bool,   set: fn(&mut HookConfig, bool))         -> SettingField<bool>          // :705
fn hook_input_field (config, ix, get: fn(&HookConfig)->String, set: fn(&mut HookConfig, SharedString)) -> SettingField<SharedString>  // :719
fn hook_method_field(config, ix) -> SettingField<SharedString>                                                                        // :734
fn hook_events_field (config, ix) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement                                      // :764
fn hook_headers_field(config, ix) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement                                      // :798
fn hook_remove_field (config, ix) -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement                                      // :884
fn add_hook_field    (config)     -> impl Fn(&RenderOptions, &mut Window, &mut App) -> AnyElement                                      // :903
```

Getter/setter pairs passed into the generic helpers (`:918-943`):
`get/set_hook_enabled`, `get/set_hook_name`, `get/set_hook_url`,
`get/set_hook_timeout`.

### 1.2 Events checkboxes (`hook_events_field`)

* Iterates `HookEvent::ALL` in display order: `TorrentAdded`, `TorrentDeleted`,
  `TorrentCompleted`, `FileSelection`, `TorrentColumns`.
* Checkbox label = `HookEvent::label()`: "Added", "Deleted", "Completed",
  "File selection", "Torrent columns".
* Click handler toggles membership in `h.events` (a `Vec`, not a set) and calls
  `cx.refresh_windows()`. Toggling **on** appends to the end, so the persisted
  order reflects click order; membership is what matters (`contains`).

### 1.3 Headers textarea (`hook_headers_field`)

* Backing state is created with `window.use_keyed_state("hook-{ix}-headers", …)`
  and holds `HookInputState { input: Entity<TextareaState>, _subscription }`.
  The subscription listens for `InputEvent::Change` and writes the parsed
  headers into the config on every change.
* **Text → map** (`set_headers_text`, `:866`): for each line — trim; skip empty;
  `split_once(':')`; trim key and value; skip empty key; insert into a fresh
  `BTreeMap` (later duplicate keys overwrite earlier ones). Lines **without a
  colon are dropped**. An empty value is allowed (`"X-Flag:"` → `"X-Flag" = ""`).
  The whole map is replaced on each change.
* **Map → text** (`headers_to_text`, `:858`): `"{k}: {v}"` per entry joined by
  `"\n"`, in `BTreeMap` (sorted-key) order.
* **Sync rule** (`:831-839`): on every render, if the textarea's current value
  differs from `headers_to_text(config)` the textarea is overwritten with the
  canonical text. The intent (per the comment) is to follow index shifts after
  add/remove, but the comparison is unconditional — see §7, item 1.

### 1.4 Element IDs (for tests / automation)

| Element | ID |
|---|---|
| Event checkbox | `hook-{ix}-event-{wire_name}` e.g. `hook-0-event-torrent_added` |
| Headers keyed state | `hook-{ix}-headers` |
| Remove button | `hook-{ix}-remove` |
| Add button | `hooks-add` |

The Enabled / Name / URL / Method / Timeout fields are not given explicit IDs by
`hook_groups`; they use whatever gpui-component assigns.

---

## 2. Page-local mutation helpers

All hook edits go through four helpers (`settings_page.rs:660-703`) that make
index staleness safe:

```rust
fn with_hook<T>(config: &ConfigArc, ix, get: impl FnOnce(&HookConfig)->T, default: T) -> T  // read lock; `default` if ix out of range
fn mutate_hook  (config: &ConfigArc, ix, f: impl FnOnce(&mut HookConfig))                  // write lock; no-op if ix out of range
fn remove_hook  (config: &ConfigArc, ix) -> bool                                            // write lock; true iff removed
fn add_hook     (config: &ConfigArc)                                                        // push HookConfig { name: "Hook {len+1}", ..Default }
```

Invariants:

* Reading or writing a hook whose index no longer exists **never panics**; reads
  yield the supplied default, writes are dropped.
* `add_hook` overrides the type-level default name (`"New hook"`) with
  `"Hook {N}"` where `N = current_len + 1`. Names are not required to be unique.
* `remove_hook` shifts all later hooks down by one; closures captured with the
  old indices keep working but now address the shifted hook until the next
  render rebuilds the groups. Add, Remove and event toggles trigger
  `cx.refresh_windows()` immediately; the switch/input/dropdown fields rely on
  the gpui-component `Settings` widget's own re-rendering.

---

## 3. Config model (persisted JSON contract)

`desktop-gpui/src/config.rs`:

```rust
#[serde(rename_all = "lowercase")]
pub enum HttpMethod { #[default] Post, Get }                       // :20

#[serde(default)]
pub struct HookConfig {                                             // :29
    pub name: String,                      // default "New hook"
    pub enabled: bool,                     // default true
    pub events: Vec<HookEvent>,            // default [TorrentAdded]
    pub method: HttpMethod,                // default Post
    pub url: String,                       // default ""
    pub timeout_secs: u64,                 // default 10
    pub headers: BTreeMap<String, String>, // default {}
    pub poll_secs: u64,                    // default 10; only used by torrent_columns
}

#[serde(default)]
pub struct RqbitDesktopConfigHooks { pub hooks: Vec<HookConfig> }   // :59  (default: empty)
```

`HookEvent` (`hooks/mod.rs:80`) serializes with `rename_all = "snake_case"`.

Shape inside `config.json`:

```json
{
  "hooks": {
    "hooks": [
      {
        "name": "notify",
        "enabled": true,
        "events": ["torrent_added", "torrent_completed", "torrent_deleted", "file_selection"],
        "method": "post",
        "url": "http://127.0.0.1:8080/hook",
        "timeout_secs": 10,
        "headers": { "Authorization": "Bearer …" },
        "poll_secs": 10
      }
    ]
  }
}
```

Rules:

* Every field of `HookConfig` and of the `hooks` section is optional on read
  (`#[serde(default)]`); `{"url": "http://x"}` is a valid hook
  (test `hook_defaults`, `config.rs:414`).
* Unknown fields are ignored (no `deny_unknown_fields`).
* Unknown event names or methods are a **deserialization error** for the whole
  config file (serde enum strictness).
* `headers` is a `BTreeMap`, so it round-trips with sorted keys.
* `timeout_secs = 0` is accepted in the file; it is clamped to 1 s at request
  time (§5) and the UI setter also clamps on edit.
* Round trip is lossless and `PartialEq`-stable (test `hooks_roundtrip`,
  `config.rs:391`).

---

## 4. Lifecycle: edit → apply → live

1. **Open**: `SettingsPage::new` snapshots the live config
   (`state.shared().read().config()`) into a page-local `ConfigArc`
   (`settings_page.rs:50-59`). `hook_groups` and every field closure operate on
   this copy only; the running session is untouched while editing.
2. **Back** without Apply: the page-local copy is dropped; all hook edits are
   discarded.
3. **Reset** (`on_reset`, `:113`): replaces the page-local config with
   `RqbitDesktopConfig::default()`, which has **zero hooks**.
4. **Apply** (`on_apply`, `:65-111`):
   1. clone the page-local config;
   2. `write_config(path, &config)` — atomic (`<path>.tmp` then rename;
      `config.rs:77`); on failure show the error and stop;
   3. `State::configure(config)` in the background (`state.rs:279`):
      * if the new config `==` the live config → no-op, `Ok`;
      * otherwise the **old session is stopped and a new one is built** (this
        applies to hook-only edits too, because the comparison is on the whole
        config), the config is written again, then the shared
        `config` + `api` are swapped under a write lock and the folder watcher
        is restarted;
      * on success the page emits `Back`; on failure it stays open with the
        error, and the new config is still adopted into shared state so the
        user's edits are not silently rolled back.
5. **Takes effect**: `HookDispatcher` holds `Arc<RwLock<SharedState>>` and reads
   `config().hooks.hooks` **at dispatch time** (`matching_hooks`,
   `dispatcher.rs:87`). The dispatcher is created once in `State::new`
   (`state.rs:241-245`, followed by `hooks.start()`) and survives session
   restarts, so the swapped config is honored by the next event with no
   dispatcher rebuild.

---

## 5. Outbound HTTP contract (`hooks/http.rs:12`)

```rust
pub(crate) async fn send(client: &reqwest::Client, hook: &HookConfig, event: HookEvent,
                         payload: &serde_json::Value) -> anyhow::Result<Option<serde_json::Value>>
```

Request:

| Aspect | Behavior |
|---|---|
| URL | `reqwest::Url::parse(&hook.url)`; unparsable URL → delivery error |
| Method `post` | `POST`, `Content-Type: application/json`, body = payload JSON |
| Method `get` | `GET`, no body, query string `?payload=<url-encoded JSON text>` |
| Always-sent header | `X-Rqbit-Event: <wire name>` (`torrent_added`, …) |
| Custom headers | each `hook.headers` entry is **appended** (`RequestBuilder::header`), after the built-ins; duplicates are allowed, so a custom `X-Rqbit-Event` does not replace the built-in one. An invalid header name/value makes the whole send fail. |
| Timeout | `max(timeout_secs, 1)` seconds, applied to the entire request including reading the response body |
| Client | one shared `reqwest::Client::new()` per dispatcher (default redirect policy, pooling) |

Response:

| Response | Result |
|---|---|
| non-2xx status | `Err` (via `error_for_status`) |
| 2xx, empty body | `Ok(None)` |
| 2xx, body is valid JSON (any value) | `Ok(Some(value))` |
| 2xx, body is not JSON | `Err("hook response is not valid JSON: …")` |

This parsing happens for **every** event, including the fire-and-forget ones
whose response is otherwise discarded; a notification endpoint that returns a
non-JSON body (e.g. plain `OK`) will be logged as a failed delivery even though
the request succeeded.

---

## 6. Events, payloads and dispatch semantics

### 6.1 Envelope (`payload.rs:29`)

Every payload is a JSON object:

```json
{ "event": "<wire name>", "timestamp_unix_secs": <u64>, ...event-specific fields }
```

`timestamp_unix_secs` is wall-clock seconds at envelope creation (0 if the
clock is before the epoch). If serialization of the inner payload fails the
envelope degrades to `{ "event": "<wire name>" }`.

`torrent` object (`TorrentInfo`, `payload.rs:13`):

```json
{ "id": <usize|null>, "name": <string|null>, "info_hash": "<hex>", "output_folder": "<path>" }
```

### 6.2 Event catalog

| Wire name | Fired from | Extra fields | Response used? |
|---|---|---|---|
| `torrent_added` | `HookDispatcher::add_and_dispatch_added` (`dispatcher.rs:155`) — both the add dialog (`local_client.rs:203`) and the autoload folder watcher (`folder_watcher.rs:203`) | `files_total: usize`, `files_selected: usize` (count of `included` files in the API response), `selected_indices: [usize] \| null` (echo of the request's `only_files`; `null` = no restriction requested) | no |
| `torrent_deleted` | `LocalClient::dispatch_deleted` (`local_client.rs:47`), called from the `Forget`/`Delete` action handler (`local_client.rs:160-188`) only after the API call succeeded; calls `hooks.forget(id)` first | `delete_files: bool` (true = files removed from disk, false = forgotten) | no |
| `torrent_completed` | completion watcher (`HookDispatcher::start`, `dispatcher.rs:289`) | none | no |
| `file_selection` | `HookDispatcher::apply_file_selection` (`dispatcher.rs:246`) while the add dialog is being prepared (`desktop_host.rs:117` ← `main_panel.rs:102`), and from `HookDispatcher::file_selection_only_files` (`dispatcher.rs`) just before the folder watcher adds an autoloaded `.torrent` (magnets are added unrestricted — their file list is not known before the add) | `torrent.id` is `null`; `files: [{ "index": usize, "name": string, "length": u64 }]` | **yes** |
| `torrent_columns` | `ColumnsProvider` poll loop (`app-daemon/src/columns.rs`), started with the session (`state.rs` `api_from_config`); every `poll_secs` | full torrent list (see §6.6); `files` arrays included only after the hook declared a file-level column | **yes** |

`torrent_added` conditions: fired only when the API returned an `id` **and**
the dispatcher did not already know that id (`is_known`); re-adding a managed
torrent is skipped. Torrents restored from persistence at startup never fire it
(they are seeded silently by the watcher). After firing, the id is "primed"
(`prime_added`) so the watcher treats it as known-and-unfinished immediately.

`torrent_deleted` conditions: torrent details (`name`, `info_hash`,
`output_folder`) are resolved **before** the delete because the handle is gone
afterwards. If that lookup fails the hook is **not fired at all** (the delete
still proceeds). When the torrent was addressed by info-hash rather than
numeric id, the payload carries `"id": 0`.

### 6.3 Fire-and-forget dispatch (`dispatch` / `spawn_deliveries`, `dispatcher.rs:101-143`)

* Matching hooks = `enabled && events.contains(event)`, snapshot taken at
  dispatch time, config order.
* One tokio task per matching hook; tasks are not awaited, not retried, and not
  cancelled on shutdown. A slow endpoint cannot delay the others. No ordering
  guarantee across hooks or across events.
* Failure → `warn!(hook, event, error, "hook delivery failed")`; success →
  `debug!`. Nothing is surfaced in the UI.

### 6.4 `file_selection` request/response (`apply_file_selection`, `dispatcher.rs:246`)

* Hooks run **sequentially in config order**, each bounded by its own timeout.
* Each request is built from the entry's **current** `included` flags, so
  several hooks compose as a pipeline.
* Response body (`FileSelectionResponse`, `payload.rs:106`) — all fields
  optional, unknown fields ignored:

  ```json
  { "include": [usize], "exclude": [usize], "include_patterns": [string], "exclude_patterns": [string] }
  ```

  Applied in this order on top of the current selection (`compute_selection`,
  `payload.rs:143`):
  1. `include` — absolute whitelist: a file is selected iff its position is
     listed (out-of-range positions are simply never matched);
  2. `exclude` — deselect listed positions (out-of-range ignored);
  3. `include_patterns` — keep a file only if it is currently selected **and**
     matches at least one pattern;
  4. `exclude_patterns` — deselect any file matching at least one pattern.

  Indices in `include`/`exclude` are **positions in the `files` array as
  sent**. The request's `index` field is `FileRow.file_index`; the add dialog
  fills rows in torrent file order so the two coincide today, but the code does
  not look rows up by `file_index`.

* Patterns (`wildcard_match`, `payload.rs:176`): `*` = any sequence (including
  `/`), `?` = any single char, **case-insensitive**, matched against the whole
  `name` (which may contain a relative path, e.g. `sample/sample.mkv`).
* Outcomes per hook:

  | Hook outcome | Effect |
  |---|---|
  | HTTP/timeout/URL error, or non-JSON body | `warn!`, selection unchanged |
  | empty body | selection unchanged |
  | JSON that does not fit the shape | `warn!("invalid file selection response")`, unchanged |
  | valid, but would leave **nothing** selected | `warn!`, **entire response ignored** |
  | valid and changes ≥1 flag | applied; hook `name` added to the return value |
  | valid, no change | applied (no-op), name not recorded |

* Return value: names of hooks that changed the selection. The UI joins them
  with `", "` into `AddTorrentEntry.hook_note`, shown as a hint in the add
  dialog; the user can still edit the selection before confirming.

### 6.6 `torrent_columns` request/response (`app-daemon/src/columns.rs`)

Implemented by `app_daemon::columns::ColumnsProvider` rather than the desktop
`HookDispatcher`, so the headless `app-daemon` binary and the desktop's
bundled web view behave identically. Built and started in `api_from_config`
from the hooks subscribed to the event; the poll task is spawned on the
session and dies with it on a settings reconfigure. The standalone daemon
gets hooks via `--column-hook <URL>` (repeatable, POST) and
`--column-hook-poll-secs <N>`.

* Poll loop: 1 s tick; each hook fires when `last_poll + max(poll_secs, 2)`
  elapsed. Polls run sequentially in config order, each bounded by its own
  timeout. The **first hook that responds successfully owns the columns** —
  hooks after the winner are not polled that tick; if the winner starts
  failing, later hooks get their turn again.
* Request: the envelope (`event`, `timestamp_unix_secs`) plus
  `torrents: [{ id, name, info_hash, output_folder, files: [{index, name,
  length}] | null }]`. `files` are resolved per torrent via
  `Api::api_torrent_details` and included only once the hook has declared a
  file-level column (payload-size guard).
* Response body (all fields optional, unknown fields ignored):

  ```json
  {
    "columns": [ { "key": "sync_status", "name": "Sync", "files": false,
                   "kind": "text" | "status", "width": 100 } ],
    "torrents": { "<info_hash>": { "sync_status": "synced" } },
    "files": { "<info_hash>": [ { "index": 0,
                                  "values": { "sync_status": "synced" } } ] }
  }
  ```

  Sanitization: columns with empty or duplicate keys are dropped (first
  wins), the column count is capped at 8, widths clamp to 40–400, values
  must be JSON scalars and are truncated to 64 chars; value keys not
  matching a declared column are ignored. An empty 2xx body means
  "connected, no columns right now".
* Snapshot: the last successful response is cached as a `ColumnsSnapshot`
  (`state` = columns + `connected` + hook name, plus the per-torrent and
  per-file value maps). On a failed poll the data is retained but
  `connected` flips to false; a reconnect flips it back.
* Consuming side:
  * `LocalClient` (`desktop-gpui/src/local_client.rs`) decorates
    `list`/`details` payloads and serves `custom_columns()` for the poll-mode
    UI; `SharedState` carries the provider so a reconfigure swaps it with
    the session.
  * The WS server (`app-daemon/src/server.rs`) pushes
    `PushUpdate::CustomColumns` (change-diffed) to session-level
    subscribers and decorates `List`/`Details` responses, giving the
    browser client the same data.
  * UI: dynamic columns are appended to the torrent and file tables only
    while `connected` is true; the status bar shows a green plug + hook name
    when connected, a gray one + "offline" when configured but unreachable,
    and nothing when no hook provides columns.
* Logging: `info!("column hook connected")` on reconnect,
  `warn!("column hook failed")` on the first failure and then at most every
  60 s (repeats at `debug!`) — this reaches both UIs' log pages through the
  shared log broadcast.

### 6.5 Completion watcher (`start` / `watch_once` / `WatcherInner::observe`)

* Polls `api_torrent_list_ext(with_stats: true)` every `WATCH_INTERVAL = 2 s`
  (`tokio::time::interval`, first tick immediate).
* State: `finished: HashMap<id, bool>` (also the "known" set) and
  `primed: HashSet<id>`.
* Per poll:
  * if `Arc::as_ptr(api)` changed (session replaced by Apply) → `reset()` and
    re-seed silently;
  * first sighting of an id → seed with its current flag, **no event**, unless
    the id was primed and is already finished (completed between add and first
    poll) → fire;
  * `false → true` transition → fire `torrent_completed`;
  * `true → false → true` (e.g. more files selected later) → fires again;
  * ids that disappeared are pruned from both maps.
* Exactly-once per transition is guaranteed by the diff; already-finished
  torrents at app start never fire.

---

## 7. Observed caveats and limitations (current behavior, not proposals)

1. **Headers textarea normalization.** The sync at
   `settings_page.rs:831-839` overwrites the textarea whenever its text differs
   from the canonical `headers_to_text` form. Because lines without a colon are
   dropped by the parser, any page re-render while a header line is still being
   typed (before its `:`) would canonicalize the text and discard the partial
   line. Whether a re-render actually occurs per keystroke depends on GPUI
   notification propagation and has not been verified at runtime; treat as a
   UX risk to confirm.
2. **Hook-only edits restart the session.** `State::configure` compares the
   whole config, so Applying a change to just a hook stops and rebuilds the
   librqbit session (`state.rs:279-324`).
3. **Positional identity.** Hooks have no stable id; index-keyed UI state
   (`hook-{ix}-headers`) is inherited by whichever hook shifts into that index
   after a removal. The sync rule in §1.3 is what keeps the display correct.
4. **GET payload size.** With `method = "get"` the entire JSON payload travels
   in the query string; a `file_selection` request for a torrent with many
   files can exceed typical URL length limits.
5. **Non-JSON 2xx bodies are logged as failures** for all events (§5).
6. **Custom headers cannot override** the built-in `X-Rqbit-Event` (they are
   appended, not replaced).
7. **No delivery guarantees**: no retries, no persistence, no UI feedback, no
   shutdown drain.
8. `selected_indices` reflects what was *requested* (`only_files`), not the
   effective selection; `files_selected` is the effective count.
9. `torrent_deleted` is skipped when the pre-delete details lookup fails, and
    reports `"id": 0` for hash-addressed deletes (§6.2).
10. `Reset` on the settings page silently removes all hooks (default config has
    none).
11. **No UI tests for `hook_groups`.** `settings_page.rs` tests cover the web
    view port and folder browsing only. Hook behavior is tested below the UI.

---

## 8. Test coverage (as of today)

| Area | Tests |
|---|---|
| Config serde | `config.rs`: `hooks_roundtrip`, `hook_defaults` |
| Selection logic | `payload.rs`: `wildcard_matching`, `apply_include_exclude`, `apply_patterns`, `empty_response_keeps_selection`, `selects_nothing_detection`, `response_deserializes_partial_json` |
| Transport | `http.rs`: `get_and_post_hit_test_server` (POST body + custom header; GET `?payload=`; empty body → `None`) |
| `torrent_columns` provider | `app-daemon/src/columns.rs`: response parsing/sanitization (full response, empty body, unknown keys, dup/empty keys + cap, width clamp + truncation), snapshot decoration, connected/failure transitions + rate limiting, `needs_files` tracking, POST round-trip over a local axum server, non-2xx error |
| Watcher | `dispatcher.rs`: `seeds_silently_on_first_observation`, `fires_on_not_finished_to_finished_transition`, `unfinishes_when_selection_grows`, `primed_torrent_completing_before_first_poll_fires`, `primed_torrent_unfinished_on_first_poll_is_quiet`, `deleted_torrents_are_pruned`, `reset_clears_everything` |
| End-to-end | `dispatcher.rs`: `file_selection_end_to_end_over_http`, `disabled_hook_is_not_contacted`, `dispatch_delivers_torrent_added_over_http` (asserts `x-rqbit-event`), `manual_add_moves_source_file`, `manual_add_keeps_source_without_move_folder` |
| Custom-column UI logic | `rqbit-ui/src/ui/utils.rs`: `status_tone` mapping tests |
| Settings UI (hooks) | none |
