//! Deterministic, no-network backend for GPUI agent end-to-end tests.
//!
//! This module is compiled only with `agent-testing`. Fixture mutations are
//! entirely in memory: even `Delete` and `UpdateOnlyFiles` never touch disk.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use futures::stream::LocalBoxStream;
use rqbit_protocol::{
    ApiAddTorrent, DhtStats, PeerStatsSnapshot, ProtoError, ProtoErrorKind, SessionStats,
    TorrentAction, TorrentDetails, TorrentFile, TorrentId, TorrentList, TorrentState, TorrentStats,
};
use rqbit_ui::client::{AddTorrentRequest, ClientResult, TorrentClient, UiAddSource};
use rqbit_ui::host::{UiCapabilities, UiHost};

#[derive(Default)]
struct FixtureData {
    torrents: BTreeMap<u64, TorrentDetails>,
    actions: Vec<(u64, TorrentAction)>,
}

/// A `?Send`, UI-thread-local client whose responses never leave the process.
pub struct FixtureClient {
    data: RefCell<FixtureData>,
}

impl FixtureClient {
    pub fn new() -> Self {
        let torrents = [
            fixture_torrent(1, "Fixture Alpha", TorrentState::Live),
            fixture_torrent(2, "Fixture Beta", TorrentState::Paused),
        ]
        .into_iter()
        .map(|torrent| (torrent.id.expect("fixture id"), torrent))
        .collect();
        Self {
            data: RefCell::new(FixtureData {
                torrents,
                actions: Vec::new(),
            }),
        }
    }

    /// Actions recorded so far; consumed by tests today and by the agent
    /// bridge once it is wired up.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn recorded_actions(&self) -> Vec<(u64, TorrentAction)> {
        self.data.borrow().actions.clone()
    }

    fn lookup<'a>(data: &'a FixtureData, id: &TorrentId) -> ClientResult<&'a TorrentDetails> {
        let id = id.as_id().ok_or_else(|| {
            ProtoError::bad_request("fixture torrents are addressable only by numeric id")
        })?;
        data.torrents.get(&id).ok_or_else(|| {
            ProtoError::new(
                ProtoErrorKind::TorrentNotFound,
                format!("fixture torrent {id} not found"),
            )
        })
    }

    fn lookup_mut<'a>(
        data: &'a mut FixtureData,
        id: &TorrentId,
    ) -> ClientResult<&'a mut TorrentDetails> {
        let id = id.as_id().ok_or_else(|| {
            ProtoError::bad_request("fixture torrents are addressable only by numeric id")
        })?;
        data.torrents.get_mut(&id).ok_or_else(|| {
            ProtoError::new(
                ProtoErrorKind::TorrentNotFound,
                format!("fixture torrent {id} not found"),
            )
        })
    }
}

#[async_trait(?Send)]
impl TorrentClient for FixtureClient {
    async fn list(&self, _with_stats: bool) -> ClientResult<TorrentList> {
        let torrents = self
            .data
            .borrow()
            .torrents
            .values()
            .cloned()
            .map(|mut torrent| {
                torrent.files = None;
                torrent
            })
            .collect();
        Ok(TorrentList { torrents })
    }

    async fn details(&self, torrent: &TorrentId) -> ClientResult<TorrentDetails> {
        Ok(Self::lookup(&self.data.borrow(), torrent)?.clone())
    }

    async fn stats(&self, torrent: &TorrentId) -> ClientResult<TorrentStats> {
        Self::lookup(&self.data.borrow(), torrent)?
            .stats
            .as_deref()
            .cloned()
            .ok_or_else(|| ProtoError::internal("fixture torrent is missing stats"))
    }

    async fn peer_stats(&self, torrent: &TorrentId) -> ClientResult<PeerStatsSnapshot> {
        Self::lookup(&self.data.borrow(), torrent)?;
        Ok(PeerStatsSnapshot::default())
    }

    async fn session_stats(&self) -> ClientResult<SessionStats> {
        Ok(SessionStats {
            uptime_seconds: 42,
            ..Default::default()
        })
    }

    async fn session_default_folder(&self) -> ClientResult<Option<String>> {
        Ok(Some("/fixture".into()))
    }

    async fn dht_stats(&self) -> ClientResult<DhtStats> {
        Err(ProtoError::new(
            ProtoErrorKind::DhtDisabled,
            "DHT is disabled in the agent fixture",
        ))
    }

    async fn action(&self, torrent: &TorrentId, action: TorrentAction) -> ClientResult<()> {
        let mut data = self.data.borrow_mut();
        let id = torrent
            .as_id()
            .ok_or_else(|| ProtoError::bad_request("fixture actions require a numeric id"))?;
        if matches!(action, TorrentAction::Forget | TorrentAction::Delete) {
            if data.torrents.remove(&id).is_none() {
                return Err(ProtoError::new(
                    ProtoErrorKind::TorrentNotFound,
                    format!("fixture torrent {id} not found"),
                ));
            }
            data.actions.push((id, action));
            return Ok(());
        }

        let details = Self::lookup_mut(&mut data, torrent)?;
        match &action {
            TorrentAction::Pause => set_torrent_state(details, TorrentState::Paused),
            TorrentAction::Start => set_torrent_state(details, TorrentState::Live),
            TorrentAction::UpdateOnlyFiles(selected) => {
                let files = details.files.as_mut().ok_or_else(|| {
                    ProtoError::internal("fixture torrent is missing its file list")
                })?;
                if selected.iter().any(|&index| index as usize >= files.len()) {
                    return Err(ProtoError::bad_request(
                        "fixture file index is out of range",
                    ));
                }
                let selected: HashSet<u64> = selected.iter().copied().collect();
                for (index, file) in files.iter_mut().enumerate() {
                    file.included = selected.contains(&(index as u64));
                }
            }
            TorrentAction::CheckFiles => {}
            TorrentAction::Forget | TorrentAction::Delete => unreachable!(),
        }
        data.actions.push((id, action));
        Ok(())
    }

    async fn add_torrent(&self, req: &AddTorrentRequest) -> ClientResult<ApiAddTorrent> {
        let UiAddSource::Magnet(magnet) = &req.source else {
            return Err(ProtoError::bad_request(
                "agent fixture accepts only deterministic magnet input",
            ));
        };
        let magnet = magnet.trim();
        let valid_hash = magnet.len() == 40 && magnet.bytes().all(|b| b.is_ascii_hexdigit());
        if !(magnet.starts_with("magnet:?") || valid_hash) {
            return Err(ProtoError::bad_request(
                "invalid fixture magnet or info hash",
            ));
        }

        let id = req.list_only.then_some(()).map_or_else(
            || {
                let data = self.data.borrow();
                data.torrents.keys().next_back().copied().unwrap_or(0) + 1
            },
            |_| 0,
        );
        let mut details = fixture_torrent(id, "Fixture Magnet", TorrentState::Paused);
        details.id = (!req.list_only).then_some(id);
        if let Some(selected) = &req.only_files {
            let files = details.files.as_mut().expect("fixture files");
            if selected.iter().any(|&index| index as usize >= files.len()) {
                return Err(ProtoError::bad_request(
                    "fixture file index is out of range",
                ));
            }
            let selected: HashSet<u64> = selected.iter().copied().collect();
            for (index, file) in files.iter_mut().enumerate() {
                file.included = selected.contains(&(index as u64));
            }
        }
        if !req.paused {
            set_torrent_state(&mut details, TorrentState::Live);
        }

        if !req.list_only {
            self.data.borrow_mut().torrents.insert(id, details.clone());
        }
        Ok(ApiAddTorrent {
            id: details.id,
            details,
            output_folder: String::new(),
        })
    }

    async fn set_rust_log(&self, _value: String) -> ClientResult<()> {
        Err(ProtoError::bad_request(
            "log configuration is unavailable in the agent fixture",
        ))
    }

    fn log_lines(&self) -> ClientResult<LocalBoxStream<'static, Vec<u8>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

/// Host capabilities deliberately exclude real settings, logs, file pickers,
/// filesystem integration, HTTP artwork, and external URL/media handlers.
pub struct FixtureHost {
    theme: RefCell<String>,
}

impl FixtureHost {
    pub fn new() -> Self {
        Self {
            theme: RefCell::new("light".into()),
        }
    }

    /// The last theme persisted through `UiHost::persist_theme`; read by the
    /// agent bridge once it is wired up.
    #[allow(dead_code)]
    pub fn theme(&self) -> String {
        self.theme.borrow().clone()
    }
}

impl UiHost for FixtureHost {
    fn capabilities(&self) -> UiCapabilities {
        UiCapabilities {
            add_torrent_files: false,
            open_folder: false,
            open_file: false,
            settings_page: false,
            logs_page: false,
        }
    }

    fn version(&self) -> String {
        "agent-fixture".into()
    }

    fn persist_theme(&self, mode: &str) {
        *self.theme.borrow_mut() = mode.into();
    }
}

fn fixture_torrent(id: u64, name: &str, state: TorrentState) -> TorrentDetails {
    let files = vec![
        TorrentFile {
            name: format!("{name} - first.txt"),
            components: vec![format!("{name} - first.txt")],
            length: 512,
            included: true,
            padding: false,
            custom: None,
        },
        TorrentFile {
            name: format!("{name} - second.txt"),
            components: vec![format!("{name} - second.txt")],
            length: 512,
            included: true,
            padding: false,
            custom: None,
        },
    ];
    let mut details = TorrentDetails {
        id: Some(id),
        info_hash: format!("{id:040x}"),
        name: Some(name.into()),
        files: Some(files),
        ..Default::default()
    };
    set_torrent_state(&mut details, state);
    details
}

fn set_torrent_state(details: &mut TorrentDetails, state: TorrentState) {
    let stats = details.stats.get_or_insert_with(|| {
        Box::new(TorrentStats {
            state,
            file_progress: vec![256, 256],
            error: None,
            progress_bytes: 512,
            uploaded_bytes: 0,
            total_bytes: 1024,
            finished: false,
            live: None,
        })
    });
    stats.state = state;
    stats.live = (state == TorrentState::Live).then_some(Default::default());
    stats.finished = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> FixtureClient {
        FixtureClient::new()
    }

    #[test]
    fn seeds_deterministic_torrents_without_exposing_list_file_paths() {
        futures::executor::block_on(async {
            let client = client();
            let list = client.list(true).await.unwrap();
            assert_eq!(list.torrents.len(), 2);
            assert_eq!(list.torrents[0].name.as_deref(), Some("Fixture Alpha"));
            assert!(list.torrents.iter().all(|torrent| torrent.files.is_none()));

            let details = client.details(&TorrentId::Id(1)).await.unwrap();
            assert_eq!(details.files.unwrap().len(), 2);
            assert!(
                client
                    .peer_stats(&TorrentId::Id(1))
                    .await
                    .unwrap()
                    .peers
                    .is_empty()
            );
            assert_eq!(
                client.dht_stats().await.unwrap_err().kind,
                ProtoErrorKind::DhtDisabled
            );
        });
    }

    #[test]
    fn pause_start_and_file_selection_mutate_only_fixture_dtos() {
        futures::executor::block_on(async {
            let client = client();
            client
                .action(&TorrentId::Id(1), TorrentAction::Pause)
                .await
                .unwrap();
            assert_eq!(
                client.stats(&TorrentId::Id(1)).await.unwrap().state,
                TorrentState::Paused
            );
            assert!(
                client
                    .stats(&TorrentId::Id(1))
                    .await
                    .unwrap()
                    .live
                    .is_none()
            );

            client
                .action(&TorrentId::Id(1), TorrentAction::Start)
                .await
                .unwrap();
            assert_eq!(
                client.stats(&TorrentId::Id(1)).await.unwrap().state,
                TorrentState::Live
            );
            client
                .action(&TorrentId::Id(1), TorrentAction::UpdateOnlyFiles(vec![1]))
                .await
                .unwrap();
            let files = client
                .details(&TorrentId::Id(1))
                .await
                .unwrap()
                .files
                .unwrap();
            assert!(!files[0].included);
            assert!(files[1].included);
            assert_eq!(client.recorded_actions().len(), 3);
        });
    }

    #[test]
    fn forget_and_delete_remove_entries_and_record_the_distinct_action() {
        futures::executor::block_on(async {
            let client = client();
            client
                .action(&TorrentId::Id(1), TorrentAction::Forget)
                .await
                .unwrap();
            client
                .action(&TorrentId::Id(2), TorrentAction::Delete)
                .await
                .unwrap();
            assert!(client.list(true).await.unwrap().torrents.is_empty());
            assert_eq!(
                client.recorded_actions(),
                vec![(1, TorrentAction::Forget), (2, TorrentAction::Delete)]
            );
            assert_eq!(
                client.details(&TorrentId::Id(1)).await.unwrap_err().kind,
                ProtoErrorKind::TorrentNotFound
            );
        });
    }

    #[test]
    fn magnet_preview_and_add_are_deterministic() {
        futures::executor::block_on(async {
            let client = client();
            let magnet = "magnet:?xt=urn:btih:0000000000000000000000000000000000000003";
            let preview = client
                .add_torrent(&AddTorrentRequest {
                    source: UiAddSource::Magnet(magnet.into()),
                    only_files: Some(vec![0]),
                    overwrite: false,
                    paused: true,
                    list_only: true,
                    source_hint: None,
                })
                .await
                .unwrap();
            assert_eq!(preview.id, None);
            assert_eq!(client.list(true).await.unwrap().torrents.len(), 2);

            let added = client
                .add_torrent(&AddTorrentRequest {
                    source: UiAddSource::Magnet(magnet.into()),
                    only_files: None,
                    overwrite: false,
                    paused: true,
                    list_only: false,
                    source_hint: None,
                })
                .await
                .unwrap();
            assert_eq!(added.id, Some(3));
            assert_eq!(client.list(true).await.unwrap().torrents.len(), 3);
        });
    }
}
