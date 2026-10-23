//! Webhook-style hooks for torrent lifecycle events.
//!
//! Hooks are configured in `config.json` (see [`crate::config::HookConfig`])
//! and managed from the settings page. Each hook subscribes to a subset of
//! [`HookEvent`]s and is contacted over HTTP with a JSON payload:
//!
//! - `POST` sends the payload as the JSON request body.
//! - `GET` sends the payload urlencoded in a `payload` query parameter.
//!
//! # Events
//!
//! - [`HookEvent::TorrentAdded`] — fired once after a torrent is actually
//!   added through the UI or the watched folder (both go through
//!   [`HookDispatcher::add_and_dispatch_added`]). Torrents restored from
//!   persisted state at app startup do not fire it, and re-adding an
//!   already-managed torrent is detected and skipped.
//! - [`HookEvent::TorrentDeleted`] — fired after a torrent is deleted (with
//!   files) or forgotten (files kept); the payload carries the `delete_files`
//!   flag.
//! - [`HookEvent::TorrentCompleted`] — detected by a background watcher that
//!   polls the session every couple of seconds and diffs the per-torrent
//!   `finished` flag. Fires exactly once per torrent on the
//!   not-finished → finished transition; torrents that were already finished
//!   when the app started never fire it.
//! - [`HookEvent::FileSelection`] — the only request/response event. While
//!   preparing the add-torrent dialog, the resolved file list is sent to the
//!   hook and the JSON response decides which files are pre-selected. The
//!   user can still adjust the selection before confirming.
//! - [`HookEvent::TorrentColumns`] — a polled request/response event: every
//!   `HookConfig::poll_secs` the torrent list (optionally with per-torrent
//!   file lists) is sent to the hook, and the response declares custom
//!   columns with per-torrent/per-file values rendered by the UIs. Handled
//!   by `app_daemon::columns::ColumnsProvider` so the browser client gets
//!   the same data over the WebSocket protocol; see that module's docs for
//!   the payload contract.
//!
//! # Payloads
//!
//! Every payload is a JSON object with `event`, `timestamp_unix_secs` and a
//! `torrent` object (`id`, `name`, `info_hash`, `output_folder`). Example for
//! `torrent_added`:
//!
//! ```json
//! {
//!   "event": "torrent_added",
//!   "timestamp_unix_secs": 1760000000,
//!   "torrent": { "id": 3, "name": "Ubuntu", "info_hash": "…", "output_folder": "/Downloads" },
//!   "files_total": 5,
//!   "files_selected": 3,
//!   "selected_indices": [0, 1, 2]
//! }
//! ```
//!
//! A `file_selection` request sends `torrent` (with `id: null`) plus a
//! `files` array of `{index, name, length}`. The response body, if present,
//! must be a JSON object with any of:
//!
//! ```json
//! {
//!   "include": [0, 2],
//!   "exclude": [1],
//!   "include_patterns": ["*.mkv"],
//!   "exclude_patterns": ["*.nfo", "*.txt"]
//! }
//! ```
//!
//! `include`/`include_patterns` are whitelists (unlisted files are
//! deselected), `exclude`/`exclude_patterns` clear matching files, and the
//! fields are applied in the order shown. Patterns support `*` and `?` and
//! match against the file name. An empty body or an error leaves the current
//! selection unchanged, and a response that would deselect everything is
//! ignored (it would otherwise add a torrent with nothing to download).

mod dispatcher;
mod http;
mod payload;

pub use dispatcher::HookDispatcher;
pub(crate) use payload::envelope;
pub use payload::{TorrentDeletedPayload, TorrentInfo};

#[cfg(test)]
pub(crate) use http::testutil;

use serde::{Deserialize, Serialize};

/// A torrent lifecycle event a hook can subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    TorrentAdded,
    TorrentDeleted,
    TorrentCompleted,
    FileSelection,
    /// Polled on a timer (see `HookConfig::poll_secs`): the app sends the
    /// torrent list to the hook and the response declares custom columns
    /// plus per-torrent / per-file values. Implemented by
    /// `app_daemon::columns::ColumnsProvider`; the UIs render the columns in
    /// the torrent and file tables and show a connectivity icon in the
    /// status bar.
    TorrentColumns,
}

impl HookEvent {
    /// All events, in display order (used by the settings UI).
    pub const ALL: [HookEvent; 5] = [
        HookEvent::TorrentAdded,
        HookEvent::TorrentDeleted,
        HookEvent::TorrentCompleted,
        HookEvent::FileSelection,
        HookEvent::TorrentColumns,
    ];

    /// The wire name of this event (also used as the payload's `event` field).
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEvent::TorrentAdded => "torrent_added",
            HookEvent::TorrentDeleted => "torrent_deleted",
            HookEvent::TorrentCompleted => "torrent_completed",
            HookEvent::FileSelection => "file_selection",
            HookEvent::TorrentColumns => "torrent_columns",
        }
    }

    /// Short label for the settings UI.
    pub fn label(&self) -> &'static str {
        match self {
            HookEvent::TorrentAdded => "Added",
            HookEvent::TorrentDeleted => "Deleted",
            HookEvent::TorrentCompleted => "Completed",
            HookEvent::FileSelection => "File selection",
            HookEvent::TorrentColumns => "Torrent columns",
        }
    }
}
