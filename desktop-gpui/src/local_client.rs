//! In-process [`TorrentClient`] implementation over the embedded librqbit
//! session. This is the desktop half of the client abstraction: it calls
//! `librqbit::Api` directly (no network) and converts results through
//! `rqbit_protocol::conv`, so the shared UI in `rqbit-ui` can render it
//! exactly like a remote daemon.
//!
//! The `Api` handle is fetched per call: `State::configure` can replace the
//! whole session when settings change, and a captured handle would keep
//! talking to the stopped session.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{StreamExt as _, stream::LocalBoxStream};
use parking_lot::RwLock;

use rqbit_protocol as proto;
use rqbit_protocol::conv;
use rqbit_protocol::{
    ApiAddTorrent, DhtStats, PeerStatsSnapshot, ProtoError, ProtoErrorKind, TorrentAction,
    TorrentDetails, TorrentId, TorrentList, TorrentStats,
};

use rqbit_ui::client::{AddTorrentRequest, ClientResult, TorrentClient};

use crate::hooks::HookDispatcher;
use crate::hooks::{HookEvent, TorrentDeletedPayload, TorrentInfo, envelope};
use crate::state::SharedState;

pub struct LocalClient {
    shared: Arc<RwLock<SharedState>>,
    hooks: Arc<HookDispatcher>,
}

impl LocalClient {
    pub fn new(shared: Arc<RwLock<SharedState>>, hooks: Arc<HookDispatcher>) -> Self {
        Self { shared, hooks }
    }

    fn api(&self) -> Arc<librqbit::Api> {
        self.shared.read().api()
    }

    /// The live custom-column snapshot, when a hook provides columns. Read
    /// per call so a settings reconfigure (which swaps the provider with the
    /// session) is picked up immediately.
    fn columns(&self) -> Option<app_daemon::columns::ColumnsSnapshot> {
        self.shared.read().columns().and_then(|p| p.snapshot())
    }

    /// Fire the `torrent_deleted` hook after a successful delete/forget.
    /// Mirrors the flow the settings/main panel used to inline.
    fn dispatch_deleted(
        &self,
        id: usize,
        name: Option<String>,
        info_hash: String,
        output_folder: String,
        delete_files: bool,
    ) {
        self.hooks.forget(id);
        self.hooks.dispatch(
            HookEvent::TorrentDeleted,
            envelope(
                HookEvent::TorrentDeleted,
                TorrentDeletedPayload {
                    torrent: TorrentInfo {
                        id: Some(id),
                        name,
                        info_hash,
                        output_folder,
                    },
                    delete_files,
                },
            ),
        );
    }
}

#[async_trait(?Send)]
impl TorrentClient for LocalClient {
    async fn list(&self, with_stats: bool) -> ClientResult<TorrentList> {
        let api = self.api();
        let mut list = TorrentList::from(
            &api.api_torrent_list_ext(librqbit::api::ApiTorrentListOpts { with_stats }),
        );
        if let Some(snapshot) = self.columns() {
            snapshot.decorate_list(&mut list);
        }
        Ok(list)
    }

    async fn details(&self, torrent: &TorrentId) -> ClientResult<TorrentDetails> {
        let api = self.api();
        let id = torrent.to_api()?;
        let resp = api
            .api_torrent_details(id)
            .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotFound))?;
        // librqbit's details response carries no trackers; the UI's Trackers
        // tab reads them from the managed torrent handle (sorted for stable
        // rendering).
        let mut trackers: Vec<String> = api
            .mgr_handle(id)
            .ok()
            .map(|h| h.shared().trackers.iter().map(|t| t.to_string()).collect())
            .unwrap_or_default();
        trackers.sort();
        let mut details = conv::torrent_details(&resp, trackers);
        if let Some(snapshot) = self.columns() {
            snapshot.decorate_details(&mut details);
        }
        Ok(details)
    }

    async fn stats(&self, torrent: &TorrentId) -> ClientResult<TorrentStats> {
        let api = self.api();
        let id = torrent.to_api()?;
        api.api_stats_v1(id)
            .map(|s| TorrentStats::from(&s))
            .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotFound))
    }

    async fn peer_stats(&self, torrent: &TorrentId) -> ClientResult<PeerStatsSnapshot> {
        let api = self.api();
        let id = torrent.to_api()?;
        api.api_peer_stats(
            id,
            librqbit::api::PeerStatsFilter {
                state: librqbit::api::PeerStatsFilterState::All,
            },
        )
        .map(|s| PeerStatsSnapshot::from(&s))
        .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotLive))
    }

    async fn session_stats(&self) -> ClientResult<proto::SessionStats> {
        let api = self.api();
        Ok(proto::SessionStats::from(&api.api_session_stats()))
    }

    async fn session_default_folder(&self) -> ClientResult<Option<String>> {
        // Read the LIVE session (not the config) so it stays correct when
        // settings change tears down and recreates the session.
        let api = self.api();
        Ok(Some(
            api.session()
                .default_output_folder()
                .to_string_lossy()
                .into_owned(),
        ))
    }

    async fn dht_stats(&self) -> ClientResult<DhtStats> {
        let api = self.api();
        api.api_dht_stats()
            .map(|s| DhtStats::from(&s))
            .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::DhtDisabled))
    }

    async fn custom_columns(&self) -> Option<proto::CustomColumnsState> {
        self.shared
            .read()
            .columns()
            .and_then(|p| p.custom_columns_state())
    }

    async fn action(&self, torrent: &TorrentId, action: TorrentAction) -> ClientResult<()> {
        let api = self.api();
        let id = torrent.to_api()?;
        match action {
            TorrentAction::Pause => {
                api.api_torrent_action_pause(id).await.map_err(|e| {
                    conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest)
                })?;
            }
            TorrentAction::Start => {
                api.api_torrent_action_start(id).await.map_err(|e| {
                    conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest)
                })?;
            }
            TorrentAction::Forget | TorrentAction::Delete => {
                let delete_files = matches!(action, TorrentAction::Delete);
                // Resolve details first — the handle is gone after deletion.
                let details = api.api_torrent_details(id).ok();
                let result = if delete_files {
                    api.api_torrent_action_delete(id).await
                } else {
                    api.api_torrent_action_forget(id).await
                };
                // Propagating the error doubles as the success check; the
                // response body is empty.
                result.map_err(|e| {
                    conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest)
                })?;
                if let Some(d) = details {
                    // Hook payloads always carry numeric ids (hash ids are a
                    // remote-only addressing form).
                    let numeric_id = match id {
                        librqbit::api::TorrentIdOrHash::Id(numeric) => numeric,
                        librqbit::api::TorrentIdOrHash::Hash(_) => 0,
                    };
                    self.dispatch_deleted(
                        numeric_id,
                        d.name,
                        d.info_hash,
                        d.output_folder,
                        delete_files,
                    );
                }
            }
            TorrentAction::UpdateOnlyFiles(files) => {
                let files: std::collections::HashSet<usize> =
                    files.iter().map(|i| *i as usize).collect();
                api.api_torrent_action_update_only_files(id, &files)
                    .await
                    .map_err(|e| {
                        conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest)
                    })?;
            }
        }
        Ok(())
    }

    async fn add_torrent(&self, req: &AddTorrentRequest) -> ClientResult<ApiAddTorrent> {
        let api = self.api();
        let add: librqbit::AddTorrent<'static> = match &req.source {
            rqbit_ui::UiAddSource::Magnet(s) => librqbit::AddTorrent::from_url(s.clone()),
            rqbit_ui::UiAddSource::Url(s) => librqbit::AddTorrent::from_url(s.clone()),
            rqbit_ui::UiAddSource::Bytes(bytes) => librqbit::AddTorrent::from_bytes(bytes.clone()),
        };
        // List-only: resolve the metadata/file list without adding (the add
        // dialog's preview); the response carries `id: None`. Going through
        // the API directly, as `add_and_dispatch_added` is for real adds.
        if req.list_only {
            let response = api
                .api_add_torrent(
                    add,
                    Some(librqbit::AddTorrentOptions {
                        list_only: true,
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest))?;
            return Ok(ApiAddTorrent::from(&response));
        }
        let only_files = req
            .only_files
            .as_ref()
            .map(|files| files.iter().map(|i| *i as usize).collect::<Vec<usize>>());
        let response = self
            .hooks
            .add_and_dispatch_added(
                &api,
                add,
                only_files,
                req.overwrite,
                req.paused,
                req.source_hint.as_deref().map(PathBuf::from),
            )
            .await
            .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::BadRequest))?;
        Ok(ApiAddTorrent::from(&response))
    }

    async fn set_rust_log(&self, value: String) -> ClientResult<()> {
        let api = self.api();
        api.api_set_rust_log(value)
            .map(|_| ())
            .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::Internal))
    }

    fn log_lines(&self) -> ClientResult<LocalBoxStream<'static, Vec<u8>>> {
        let api = self.api();
        let stream = api
            .api_log_lines_stream()
            .map_err(|e| ProtoError::internal(format!("log stream unavailable: {e}")))?;
        Ok(Box::pin(stream.map(|item| match item {
            Ok(bytes) => bytes.to_vec(),
            Err(e) => format!("--- log stream lagged: {e} ---").into_bytes(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder_watcher::tests::torrent_bytes;

    /// `list_only` resolves the file list without adding the torrent to the
    /// session — the add dialog's preview contract (the response carries
    /// `id: None`).
    #[tokio::test]
    async fn list_only_resolves_without_adding() {
        let session = librqbit::Session::new_with_opts(
            std::env::temp_dir(),
            librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let api = Arc::new(librqbit::Api::new(session, None, None));
        let shared = Arc::new(RwLock::new(SharedState::for_test(
            crate::config::RqbitDesktopConfig::default(),
            api.clone(),
        )));
        let hooks = Arc::new(HookDispatcher::new(
            shared.clone(),
            tokio::runtime::Handle::current(),
        ));
        let client = LocalClient::new(shared, hooks);

        let response = client
            .add_torrent(&AddTorrentRequest {
                source: rqbit_ui::UiAddSource::Bytes(torrent_bytes()),
                only_files: None,
                overwrite: false,
                paused: false,
                list_only: true,
                source_hint: None,
            })
            .await
            .unwrap();

        assert!(response.id.is_none(), "list-only response carries no id");
        assert!(
            response
                .details
                .files
                .as_ref()
                .is_some_and(|files| !files.is_empty()),
            "list-only response resolves the file list"
        );
        assert_eq!(
            api.api_torrent_list_ext(librqbit::api::ApiTorrentListOpts { with_stats: false })
                .torrents
                .len(),
            0,
            "list-only must not add the torrent to the session"
        );
    }
}
