//! Integration tests: the media routes against a real session with a real
//! (seeded-from-disk) torrent — full-file streaming, HTTP ranges, the M3U8
//! playlist, and the error paths.

use std::sync::Arc;
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ConnectionOptions, Session, SessionOptions, create_torrent,
};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};

use app_daemon::server::{ApiSlot, router};

/// A session with one fully-downloaded single-file torrent (`movie.mp4`),
/// created from the on-disk file like a real completed download.
async fn seeded_movie() -> (Arc<Api>, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!(
        "rqbit-media-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file_bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("movie.mp4"), &file_bytes).unwrap();

    let torrent_bytes = create_torrent(
        &dir.join("movie.mp4"),
        Default::default(),
        &BlockingSpawner::new(1),
    )
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
    (api, file_bytes)
}

#[tokio::test]
async fn media_routes_serve_stream_ranges_and_playlist() {
    let (api, file_bytes) = seeded_movie().await;
    let file_len = file_bytes.len() as u64;

    let app = router(ApiSlot::new(api), None, None, None, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let stream_url = format!("{base}/torrents/0/stream/0/movie.mp4");

    // Playlist: the media file listed, pointing at the stream route.
    let resp = client
        .get(format!("{base}/torrents/0/playlist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(
        body,
        format!("#EXTM3U\r\nhttp://{addr}/torrents/0/stream/0/movie.mp4")
    );

    // Full file: 200 with Content-Length and the exact bytes.
    let resp = client.get(&stream_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get(CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        file_len.to_string()
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], &file_bytes[..]);

    // Bounded range: 206 with the exact slice.
    let resp = client
        .get(&stream_url)
        .header(RANGE, "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap().to_str().unwrap(),
        format!("bytes 100-199/{file_len}")
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], &file_bytes[100..200]);

    // Open-ended range: 206 through EOF.
    let resp = client
        .get(&stream_url)
        .header(RANGE, format!("bytes={}-", file_len - 50))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get(CONTENT_RANGE).unwrap().to_str().unwrap(),
        format!("bytes {}-{}/{}", file_len - 50, file_len - 1, file_len)
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], &file_bytes[(file_len - 50) as usize..]);

    // Out-of-bounds range: 416.
    let resp = client
        .get(&stream_url)
        .header(RANGE, format!("bytes={}-", file_len + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 416);

    // Unknown torrent: 404 on both routes.
    for url in [
        format!("{base}/torrents/123/playlist"),
        format!("{base}/torrents/123/stream/0/movie.mp4"),
    ] {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 404, "{url}");
    }

    server.abort();
}
