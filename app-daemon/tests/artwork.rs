//! Integration test: the artwork route against a real session with a real
//! seeded video file — the full preview pipeline (provider → ffmpeg frame
//! extraction → HTTP route → PNG bytes). Skips when ffmpeg is unavailable.

use std::sync::Arc;
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ConnectionOptions, Session, SessionOptions, create_torrent,
};
use reqwest::header::CONTENT_TYPE;

use app_daemon::artwork::ArtworkProvider;
use app_daemon::server::{ApiSlot, router};

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[tokio::test]
async fn artwork_route_serves_a_preview_frame() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping artwork route test");
        return;
    }

    let dir = std::env::temp_dir().join(format!(
        "rqbit-artwork-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // A real 2-second video, named like a release so the test also proves
    // the preview path (MyShows is disabled below; the file is the source).
    let video = dir.join("Sample.Show.S01E01.1080p.WEB.h264.mp4");
    let encoded = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&video)
        .status()
        .unwrap()
        .success();
    assert!(encoded, "ffmpeg could not encode the test video");

    let torrent_bytes = create_torrent(&video, Default::default(), &BlockingSpawner::new(1))
        .await
        .unwrap()
        .as_bytes()
        .unwrap();

    let session = Session::new_with_opts(
        dir,
        SessionOptions {
            dht: None,
            persistence: None,
            listen: None,
            connect: Some(ConnectionOptions::default()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let api = Arc::new(Api::new(session, None, None));
    api.api_add_torrent(
        AddTorrent::from_bytes(torrent_bytes.to_vec()),
        Some(AddTorrentOptions {
            overwrite: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    // Wait for the initial check to mark the existing file complete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let finished = api
            .api_stats_v1(librqbit::api::TorrentIdOrHash::Id(0))
            .map(|s| s.finished)
            .unwrap_or(false);
        if finished {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "torrent never finished checking"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Hermetic provider: no column hooks, MyShows disabled — the preview is
    // the only source. The artwork route itself waits on the worker for the
    // lazy ffmpeg extraction, so a single request should suffice.
    let artwork = ArtworkProvider::new(ApiSlot::new(api.clone()), None, false);
    artwork.start();
    let app = router(ApiSlot::new(api.clone()), None, None, Some(artwork), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let list = api.api_torrent_list_ext(Default::default());
    let hash = list.torrents[0].info_hash.clone();

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/torrents/by-hash/{hash}/artwork");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{url}");
    assert_eq!(
        resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap(),
        "image/png"
    );
    let body = resp.bytes().await.unwrap();
    assert!(
        body.starts_with(&[0x89, b'P', b'N', b'G']),
        "expected PNG bytes, got {} bytes",
        body.len()
    );

    // A second request serves the cached bytes again.
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    server.abort();
}
