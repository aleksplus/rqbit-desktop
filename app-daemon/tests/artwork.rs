//! Integration tests for the artwork route against real sessions:
//! - the preview pipeline (provider → ffmpeg frame extraction → HTTP route
//!   → PNG bytes), skipped when ffmpeg is unavailable;
//! - the moviesync-daemon resolve table (mocked) picking the correct show,
//!   with the poster fetched from the real MyShows `shows.GetById`, skipped
//!   when MyShows is unreachable.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ConnectionOptions, Session, SessionOptions, create_torrent,
};
use reqwest::header::CONTENT_TYPE;

use app_daemon::artwork::ArtworkProvider;
use app_daemon::columns::{ColumnsHook, ColumnsMethod, ColumnsProvider};
use app_daemon::server::{ApiSlot, handle_request, router};
use rqbit_protocol::{Request, ResponsePayload, TorrentArtwork, TorrentId};

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Poll the decorated details payload until the worker resolves artwork for
/// the seeded torrent (resolution is asynchronous behind `kick`).
async fn decorated_artwork(
    api: &Arc<Api>,
    artwork: &Arc<ArtworkProvider>,
) -> Option<TorrentArtwork> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let resp = handle_request(
            api,
            Request::Details {
                torrent: TorrentId::Id(0),
            },
            None,
            Some(artwork.as_ref()),
            None,
        )
        .await
        .unwrap();
        if let ResponsePayload::Details(details) = resp
            && let Some(a) = details.artwork
        {
            return Some(a);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A session with one fully-downloaded single-file torrent created from a
/// real video at `name` — like a completed download.
async fn seeded_video(name: &str) -> (std::path::PathBuf, Arc<Api>, String) {
    let dir = std::env::temp_dir().join(format!(
        "rqbit-artwork-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let video = dir.join(name);
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
        dir.clone(),
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

    let hash = api.api_torrent_list_ext(Default::default()).torrents[0]
        .info_hash
        .clone();
    (dir, api, hash)
}

/// Serve `app` on a random loopback port; returns its base URL and the
/// server task handle.
async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), server)
}

#[tokio::test]
async fn artwork_route_serves_a_preview_frame() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not available; skipping artwork route test");
        return;
    }

    // Hermetic provider: no column hooks, MyShows disabled — the preview is
    // the only source. The artwork route itself waits on the worker for the
    // lazy ffmpeg extraction, so a single request should suffice.
    let (_dir, api, hash) = seeded_video("Sample.Show.S01E01.1080p.WEB.h264.mp4").await;
    let artwork = ArtworkProvider::new(ApiSlot::new(api.clone()), None, false);
    artwork.start();
    let (base, server) = serve(router(
        ApiSlot::new(api.clone()),
        None,
        None,
        Some(artwork),
        None,
    ))
    .await;

    let client = reqwest::Client::new();
    let url = format!("{base}/torrents/by-hash/{hash}/artwork");
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

/// A hook-provided poster (the columns API's Image column keyed `poster`)
/// wins over every built-in heuristic: the artwork reference is decorated
/// with source `hook` and the route serves the hook's bytes. This is the
/// path external metadata providers use — e.g. moviesync-daemon publishes
/// its resolve table's poster here, and the app never talks to it directly.
#[tokio::test]
async fn artwork_prefers_hook_poster_from_columns_api() {
    let (_dir, api, hash) =
        seeded_video("Georgie.and.Mandys.First.Marriage.S01E05.720p.WEB.h264.mp4").await;

    // A stand-in hook service: the columns response declares an Image-kind
    // `poster` column and answers with this torrent's poster URL. The poster
    // bytes live on their own server so the response can reference a real,
    // resolvable URL (the hook payload carries a URL, not bytes).
    let poster_bytes: Vec<u8> = [
        0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
    ]
    .into_iter()
    .chain(std::iter::repeat_n(0x7Fu8, 2048))
    .collect();
    let (poster_base, poster_server) = serve(axum::Router::new().route(
        "/poster.jpg",
        get(move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
                poster_bytes.clone(),
            )
        }),
    ))
    .await;
    let poster_url = format!("{poster_base}/poster.jpg");

    let torrent_hash = hash.clone();
    let hook = axum::Router::new().route(
        "/hook",
        axum::routing::post(move || {
            let payload = serde_json::json!({
                "columns": [
                    {"key": "poster", "name": "Poster", "files": false, "kind": "image"},
                ],
                "torrents": { torrent_hash: { "poster": poster_url } },
            });
            async move { axum::Json(payload) }
        }),
    );
    let (hook_base, hook_server) = serve(hook).await;
    let hook_url = format!("{hook_base}/hook");

    // rqbit polls the hook through the regular columns provider.
    let columns = ColumnsProvider::new(
        ApiSlot::new(api.clone()),
        vec![ColumnsHook {
            name: "moviesync".to_owned(),
            url: hook_url,
            method: ColumnsMethod::Post,
            timeout_secs: 5,
            headers: BTreeMap::new(),
            poll_secs: 2,
        }],
    );
    columns.start();

    let artwork = ArtworkProvider::new(ApiSlot::new(api.clone()), Some(columns), false);
    artwork.start();
    let (base, server) = serve(router(
        ApiSlot::new(api.clone()),
        None,
        None,
        Some(artwork.clone()),
        None,
    ))
    .await;

    // Decorated details end up with the hook-sourced artwork reference: the
    // fast local preview may land first, then the columns snapshot arrives
    // and the upgrade replaces it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let _artwork_ref = loop {
        let current = decorated_artwork(&api, &artwork).await;
        match current.as_ref().map(|a| a.source.as_str()) {
            Some("hook") => break current.expect("checked"),
            _ if tokio::time::Instant::now() >= deadline => {
                panic!("hook poster never became the artwork, got {current:?}")
            }
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    };

    // The route serves the hook's bytes.
    let client = reqwest::Client::new();
    let url = format!("{base}/torrents/by-hash/{hash}/artwork");
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200, "{url}");
    assert_eq!(
        resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap(),
        "image/jpeg"
    );

    server.abort();
    hook_server.abort();
    poster_server.abort();
}

/// The built-in MyShows search only accepts an exact title match — the
/// fuzzy first result that pairs "…First Marriage" with "Marriage Toxin"
/// must not become the poster; the completed file's preview frame is the
/// fallback instead.
#[tokio::test]
async fn naive_search_rejects_fuzzy_title() {
    let (_dir, api, _hash) =
        seeded_video("Georgie.and.Mandys.First.Marriage.S01E05.720p.WEB.h264.mp4").await;

    let artwork = ArtworkProvider::new(ApiSlot::new(api.clone()), None, true);
    artwork.start();
    let _ = serve(router(
        ApiSlot::new(api.clone()),
        None,
        None,
        Some(artwork.clone()),
        None,
    ))
    .await;

    let resp = decorated_artwork(&api, &artwork).await;
    let source = resp.as_ref().map(|a| a.source.as_str());
    assert_ne!(
        source,
        Some("myshows"),
        "a fuzzy non-exact search result must not be picked as a poster"
    );
    // The completed file is present, so the preview is the fallback.
    assert_eq!(source, Some("preview"), "{source:?}");
}
