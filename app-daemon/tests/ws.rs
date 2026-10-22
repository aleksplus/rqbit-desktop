//! Integration tests: request mapping against a quiet session, and a real
//! WebSocket round-trip through the axum server.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use librqbit::{Api, ConnectionOptions, Session, SessionOptions};
use rqbit_protocol as proto;
use rqbit_protocol::codec;
use rqbit_protocol::{ClientMessage, ServerMessage};

/// A minimal real session: no DHT, no listener, no persistence.
async fn quiet_api() -> Arc<Api> {
    let session = Session::new_with_opts(
        std::env::temp_dir(),
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
    Arc::new(Api::new(session, None, None))
}

fn request(id: u64, req: proto::Request) -> ClientMessage {
    ClientMessage::Request { id, req }
}

/// `handle_request` maps the shared UI's API surface 1:1: the happy paths
/// return payloads and the error paths carry the right error kind.
#[tokio::test]
async fn handle_request_maps_api_surface() {
    let api = quiet_api().await;

    // Session stats: an empty session still reports speeds/uptime.
    let resp = server::handle_request(&api, proto::Request::SessionStats, None)
        .await
        .unwrap();
    assert!(matches!(resp, proto::ResponsePayload::SessionStats(_)));

    // List: empty session → empty list.
    let resp = server::handle_request(&api, proto::Request::List { with_stats: true }, None)
        .await
        .unwrap();
    match resp {
        proto::ResponsePayload::List(list) => assert!(list.torrents.is_empty()),
        other => panic!("unexpected payload: {other:?}"),
    }

    // DHT is disabled in this session → DhtDisabled error kind.
    let err = server::handle_request(&api, proto::Request::DhtStats, None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, proto::ProtoErrorKind::DhtDisabled);

    // Addressing by a well-formed-but-unknown info hash → TorrentNotFound.
    let bogus = proto::TorrentId::Hash("0".repeat(40));
    let err = server::handle_request(
        &api,
        proto::Request::Details {
            torrent: bogus.clone(),
        },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, proto::ProtoErrorKind::TorrentNotFound);

    // Actions on unknown torrents fail, but with a decodable wire error.
    let err = server::handle_request(
        &api,
        proto::Request::Action {
            torrent: bogus,
            action: proto::TorrentAction::Pause,
        },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, proto::ProtoErrorKind::TorrentNotFound);

    // Malformed ids (bad hash length) → BadRequest.
    let err = server::handle_request(
        &api,
        proto::Request::Stats {
            torrent: proto::TorrentId::Hash("nope".into()),
        },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, proto::ProtoErrorKind::BadRequest);
}

async fn spawn_server() -> String {
    let (addr, _port) = spawn_server_addr().await;
    format!("ws://{addr}/ws")
}

async fn spawn_server_addr() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let api = quiet_api().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app_daemon::server::router(api, None, None);
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, handle)
}

/// The daemon serves the embedded web client itself, on its own port at `/` —
/// the same slot the classic React webui occupies in the upstream `rqbit
/// server`, whose relative `./pkg/…` asset paths resolve to `/pkg/…`. Whether
/// the real wasm payload is embedded depends on `make web-client-build`;
/// either way the routes answer with the right content types.
#[tokio::test]
async fn web_client_is_served_by_daemon() {
    let (addr, _handle) = spawn_server_addr().await;

    let (status, body, content_type) = http_get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains("rqbit"), "page should name rqbit");

    // The wasm-bindgen payload (real or placeholder) under the HTML root.
    let (status, _body, content_type) = http_get(addr, "/pkg/web_gpui.js").await;
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/javascript"));

    let (status, _body, content_type) = http_get(addr, "/pkg/web_gpui_bg.wasm").await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/wasm");

    // Unknown paths (including the old /web root) are a clean 404.
    for path in ["/nope", "/web/"] {
        let (status, _body, _ct) = http_get(addr, path).await;
        assert_eq!(status, 404, "GET {path}");
    }
}

/// Minimal HTTP/1.1 GET over raw TCP — enough to assert status line,
/// content-type header, and body without pulling an HTTP client dependency.
async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    // The body may be binary (the wasm payload); read bytes, decode the
    // header section as ASCII, and keep the body as lossy text (assertions
    // only grep for ASCII markers).
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let body = String::from_utf8_lossy(&buf[header_end..]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = head.as_str();
    let content_type = headers
        .lines()
        .find_map(|l| {
            l.split_once(": ")
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_default();
    (status, body, content_type)
}

/// Full binary-protocol round-trip over a real WebSocket: Ping→Pong,
/// request→typed error response, and Subscribe→pushed TorrentList.
#[tokio::test]
async fn websocket_roundtrip() {
    let url = spawn_server().await;
    let (ws, _resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws.split();

    // Ping → Pong.
    write
        .send(tokio_tungstenite::tungstenite::Message::binary(
            codec::encode_client(&ClientMessage::Ping).unwrap(),
        ))
        .await
        .unwrap();

    let mut got_pong = false;
    while !got_pong {
        let msg = read.next().await.unwrap().unwrap();
        if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = msg {
            match codec::decode_server(&bytes).unwrap() {
                ServerMessage::Pong => got_pong = true,
                other => panic!("expected pong, got {other:?}"),
            }
        }
    }

    // Subscribe (session level), then expect a pushed TorrentList quickly.
    write
        .send(tokio_tungstenite::tungstenite::Message::binary(
            codec::encode_client(&ClientMessage::Subscribe { torrent: None }).unwrap(),
        ))
        .await
        .unwrap();

    let got_list = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let msg = read.next().await.unwrap().unwrap();
            let tokio_tungstenite::tungstenite::Message::Binary(bytes) = msg else {
                continue;
            };
            match codec::decode_server(&bytes).unwrap() {
                ServerMessage::Push(proto::PushUpdate::TorrentList(_)) => return true,
                _ => continue,
            }
        }
    })
    .await;
    assert!(got_list.is_ok(), "no TorrentList push within 5s");

    // A request with a correlated id → typed error response.
    write
        .send(tokio_tungstenite::tungstenite::Message::binary(
            codec::encode_client(&request(
                42,
                proto::Request::Details {
                    torrent: proto::TorrentId::Hash("0".repeat(40)),
                },
            ))
            .unwrap(),
        ))
        .await
        .unwrap();

    let mut got_response = false;
    while !got_response {
        let msg = read.next().await.unwrap().unwrap();
        let tokio_tungstenite::tungstenite::Message::Binary(bytes) = msg else {
            continue;
        };
        match codec::decode_server(&bytes).unwrap() {
            ServerMessage::Response { id, result } => {
                assert_eq!(id, 42);
                assert_eq!(
                    result.unwrap_err().kind,
                    proto::ProtoErrorKind::TorrentNotFound
                );
                got_response = true;
            }
            _ => continue, // pushes may interleave
        }
    }
}

use app_daemon::server;
