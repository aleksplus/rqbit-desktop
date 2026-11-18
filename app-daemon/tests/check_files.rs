//! Integration coverage for forcing an existing torrent through a full file check.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ConnectionOptions, Session, SessionOptions,
    TorrentStatsState, create_torrent, spawn_utils::BlockingSpawner,
};
use rqbit_protocol as proto;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "rqbit-check-files-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn wait_for_state(api: &Api, state: TorrentStatsState) -> librqbit::TorrentStats {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let stats = api
            .api_stats_v1(librqbit::api::TorrentIdOrHash::Id(0))
            .unwrap();
        if std::mem::discriminant(&stats.state) == std::mem::discriminant(&state) {
            return stats;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "torrent did not reach {state:?}: {:?}",
            stats.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn check_files_detects_corruption_and_preserves_torrent_state() {
    let dir = TestDir::new();
    let file_path = dir.path().join("movie.bin");
    let file_bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file_path, &file_bytes).unwrap();

    let torrent_bytes = create_torrent(&file_path, Default::default(), &BlockingSpawner::new(1))
        .await
        .unwrap()
        .as_bytes()
        .unwrap();

    let session = Session::new_with_opts(
        dir.path().to_owned(),
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            disable_local_service_discovery: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let api = Arc::new(Api::new(session, None, None));
    let added = api
        .api_add_torrent(
            AddTorrent::from_bytes(torrent_bytes),
            Some(AddTorrentOptions {
                paused: true,
                only_files: Some(vec![0]),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(added.id, Some(0));
    let initial = wait_for_state(&api, TorrentStatsState::Paused).await;
    assert!(
        initial.finished,
        "the valid existing file should be complete"
    );
    let before = api
        .api_torrent_details(librqbit::api::TorrentIdOrHash::Id(0))
        .unwrap();

    let corrupted: Vec<u8> = file_bytes.iter().map(|byte| byte ^ 0xff).collect();
    std::fs::write(&file_path, &corrupted).unwrap();

    let response = app_daemon::server::handle_request(
        &api,
        proto::Request::Action {
            torrent: proto::TorrentId::Id(0),
            action: proto::TorrentAction::CheckFiles,
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(response, proto::ResponsePayload::Empty));

    let after = wait_for_state(&api, TorrentStatsState::Paused).await;
    assert!(!after.finished, "the corrupted file must not stay complete");
    assert_eq!(after.progress_bytes, 0);
    assert_eq!(std::fs::read(&file_path).unwrap(), corrupted);

    let details = api
        .api_torrent_details(librqbit::api::TorrentIdOrHash::Id(0))
        .unwrap();
    assert_eq!(details.id, Some(0));
    assert_eq!(details.output_folder, before.output_folder);
    let included: Vec<bool> = details
        .files
        .unwrap()
        .into_iter()
        .map(|file| file.included)
        .collect();
    assert_eq!(included, vec![true]);
    assert!(
        api.session()
            .get(librqbit::api::TorrentIdOrHash::Id(0))
            .unwrap()
            .is_paused()
    );

    // A torrent that was running before the check resumes automatically.
    api.api_torrent_action_start(librqbit::api::TorrentIdOrHash::Id(0))
        .await
        .unwrap();
    wait_for_state(&api, TorrentStatsState::Live).await;
    app_daemon::server::handle_request(
        &api,
        proto::Request::Action {
            torrent: proto::TorrentId::Id(0),
            action: proto::TorrentAction::CheckFiles,
        },
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let resumed = wait_for_state(&api, TorrentStatsState::Live).await;
    assert!(!resumed.finished);
    assert!(
        !api.session()
            .get(librqbit::api::TorrentIdOrHash::Id(0))
            .unwrap()
            .is_paused()
    );
}
