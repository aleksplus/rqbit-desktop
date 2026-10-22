//! WebSocket protocol server: one connection = one actor.
//!
//! Requests are dispatched straight to `librqbit::Api` (the Rust layer —
//! the REST handlers are not involved). Pushes are produced by a sampler
//! task that re-reads the subscribed state, encodes it to wire bytes, and
//! sends only when the bytes changed: torrent stats (speeds, progress)
//! change continuously, so event hooks alone would not carry live updates,
//! while a change-diff avoids sending static state every tick.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use librqbit::Api;
use librqbit::api::{ApiTorrentListOpts, PeerStatsFilter, PeerStatsFilterState};
use librqbit::tracing_subscriber_config_utils::LineBroadcast;
use rqbit_protocol as proto;
use rqbit_protocol::codec;
use rqbit_protocol::conv;
use rqbit_protocol::types::TorrentList;
use rqbit_protocol::{
    AddSource, ClientMessage, CustomColumnsState, PushUpdate, Request, ResponsePayload,
    ServerMessage,
};
use tokio::sync::{broadcast, mpsc};

use crate::columns::ColumnsProvider;

/// How often subscribed state is sampled for pushes.
const PUSH_INTERVAL: Duration = Duration::from_millis(500);

/// Execute one request against the Api, decorating list/details responses
/// with hook-provided custom column values when a provider is present.
/// Public so integration tests can exercise the mapping without a socket.
pub async fn handle_request(
    api: &Api,
    req: Request,
    columns: Option<&ColumnsProvider>,
) -> Result<ResponsePayload, proto::ProtoError> {
    use proto::ProtoErrorKind;
    Ok(match req {
        Request::List { with_stats } => {
            let mut list =
                TorrentList::from(&api.api_torrent_list_ext(ApiTorrentListOpts { with_stats }));
            if let Some(snapshot) = columns.and_then(|c| c.snapshot()) {
                snapshot.decorate_list(&mut list);
            }
            ResponsePayload::List(Box::new(list))
        }
        Request::Details { torrent } => {
            let id = torrent.to_api()?;
            let resp = api.api_torrent_details(id).map_err(|e| {
                conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotFound)
            })?;
            // librqbit's details response carries no trackers; resolve them
            // from the managed torrent handle like the desktop client does.
            let mut trackers: Vec<String> = api
                .mgr_handle(id)
                .ok()
                .map(|h| h.shared().trackers.iter().map(|t| t.to_string()).collect())
                .unwrap_or_default();
            trackers.sort();
            let mut details = conv::torrent_details(&resp, trackers);
            if let Some(snapshot) = columns.and_then(|c| c.snapshot()) {
                snapshot.decorate_details(&mut details);
            }
            ResponsePayload::Details(Box::new(details))
        }
        Request::Stats { torrent } => {
            let id = torrent.to_api()?;
            let stats = api.api_stats_v1(id).map_err(|e| {
                conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotFound)
            })?;
            ResponsePayload::Stats(Box::new(proto::TorrentStats::from(&stats)))
        }
        Request::PeerStats { torrent } => {
            let id = torrent.to_api()?;
            let peers = api
                .api_peer_stats(
                    id,
                    PeerStatsFilter {
                        state: PeerStatsFilterState::All,
                    },
                )
                .map_err(|e| {
                    conv::proto_error_from_api_error(&e, ProtoErrorKind::TorrentNotLive)
                })?;
            ResponsePayload::PeerStats(proto::PeerStatsSnapshot::from(&peers))
        }
        Request::SessionStats => ResponsePayload::SessionStats(Box::new(
            proto::SessionStats::from(&api.api_session_stats()),
        )),
        Request::SessionInfo => ResponsePayload::SessionInfo(Box::new(proto::SessionInfo {
            default_download_folder: api
                .session()
                .default_output_folder()
                .to_string_lossy()
                .into_owned(),
        })),
        Request::DhtStats => {
            let stats = api
                .api_dht_stats()
                .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::DhtDisabled))?;
            ResponsePayload::DhtStats(proto::DhtStats::from(&stats))
        }
        Request::Action { torrent, action } => {
            let id = torrent.to_api()?;
            match action {
                proto::TorrentAction::Pause => {
                    api.api_torrent_action_pause(id).await.map_err(api_err)?;
                }
                proto::TorrentAction::Start => {
                    api.api_torrent_action_start(id).await.map_err(api_err)?;
                }
                proto::TorrentAction::Forget => {
                    api.api_torrent_action_forget(id).await.map_err(api_err)?;
                }
                proto::TorrentAction::Delete => {
                    api.api_torrent_action_delete(id).await.map_err(api_err)?;
                }
                proto::TorrentAction::UpdateOnlyFiles(files) => {
                    let files: std::collections::HashSet<usize> =
                        files.iter().map(|i| *i as usize).collect();
                    api.api_torrent_action_update_only_files(id, &files)
                        .await
                        .map_err(api_err)?;
                }
            }
            ResponsePayload::Empty
        }
        Request::AddTorrent {
            source,
            only_files,
            overwrite,
            paused,
            list_only,
        } => {
            let add: librqbit::AddTorrent<'static> = add_source_to_add(&source);
            let opts = librqbit::AddTorrentOptions {
                only_files: only_files
                    .map(|files| files.iter().map(|i| *i as usize).collect::<Vec<usize>>()),
                overwrite,
                paused,
                list_only,
                ..Default::default()
            };
            let resp = api
                .api_add_torrent(add, Some(opts))
                .await
                .map_err(api_err)?;
            ResponsePayload::Added(Box::new(proto::ApiAddTorrent::from(&resp)))
        }
        Request::SetRustLog(value) => {
            api.api_set_rust_log(value)
                .map_err(|e| conv::proto_error_from_api_error(&e, ProtoErrorKind::Internal))?;
            ResponsePayload::Empty
        }
    })
}

fn add_source_to_add(source: &AddSource) -> librqbit::AddTorrent<'static> {
    match source {
        // `from_url` covers magnet and http(s) identically (same path the
        // desktop dialog uses).
        AddSource::Magnet(s) => librqbit::AddTorrent::from_url(s.clone()),
        AddSource::Url(s) => librqbit::AddTorrent::from_url(s.clone()),
    }
}

fn api_err(e: librqbit::ApiError) -> proto::ProtoError {
    // Map by HTTP status; a 404 from an action means the torrent is gone
    // (TorrentNotFound). BadRequest remains the fallback for 400s.
    conv::proto_error_from_api_error(&e, proto::ProtoErrorKind::TorrentNotFound)
}

/// Subscription state for one connection, shared between the reader loop and
/// the push tasks.
#[derive(Default)]
struct ConnState {
    session_level: bool,
    torrents: HashSet<proto::TorrentId>,
    logs: bool,
}

/// Serve one WebSocket connection until the client or the socket goes away.
pub async fn run_connection(
    socket: WebSocket,
    api: Arc<Api>,
    line_broadcast: Option<LineBroadcast>,
    columns: Option<Arc<ColumnsProvider>>,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();

    // Writer task: serialize all outgoing frames through one queue.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(parking_lot::Mutex::new(ConnState::default()));

    // Push sampler: session-level list + stats, change-diffed.
    {
        let state = state.clone();
        let api = api.clone();
        let columns = columns.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PUSH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_list: Vec<u8> = Vec::new();
            let mut last_stats: Vec<u8> = Vec::new();
            let mut last_columns: Vec<u8> = Vec::new();
            loop {
                interval.tick().await;
                if !state.lock().session_level {
                    continue;
                }
                let mut list = TorrentList::from(
                    &api.api_torrent_list_ext(ApiTorrentListOpts { with_stats: true }),
                );
                // Attach hook-provided custom column values so the change
                // diff picks up column updates too.
                let columns_snapshot = columns.as_ref().and_then(|c| c.snapshot());
                if let Some(snapshot) = &columns_snapshot {
                    snapshot.decorate_list(&mut list);
                }
                if let Ok(bytes) = codec::encode_server(&ServerMessage::Push(
                    PushUpdate::TorrentList(Box::new(list)),
                )) && bytes != last_list
                {
                    last_list = bytes.clone();
                    if tx.send(WsMessage::Binary(bytes.into())).is_err() {
                        break;
                    }
                }
                let stats = proto::SessionStats::from(&api.api_session_stats());
                if let Ok(bytes) = codec::encode_server(&ServerMessage::Push(
                    PushUpdate::SessionStats(Box::new(stats)),
                )) && bytes != last_stats
                {
                    last_stats = bytes.clone();
                    if tx.send(WsMessage::Binary(bytes.into())).is_err() {
                        break;
                    }
                }
                if let Some(snapshot) = &columns_snapshot {
                    let state = CustomColumnsState {
                        columns: snapshot.state.columns.clone(),
                        connected: snapshot.state.connected,
                        source: snapshot.state.source.clone(),
                    };
                    if let Ok(bytes) = codec::encode_server(&ServerMessage::Push(
                        PushUpdate::CustomColumns(Box::new(state)),
                    )) && bytes != last_columns
                    {
                        last_columns = bytes.clone();
                        if tx.send(WsMessage::Binary(bytes.into())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Log forwarder: subscribes to the session-wide log broadcast and
    // forwards raw line bytes to connections that asked for them.
    if let Some(line_broadcast) = line_broadcast {
        let state = state.clone();
        let tx = tx.clone();
        let mut log_rx: broadcast::Receiver<bytes::Bytes> = line_broadcast.subscribe();
        tokio::spawn(async move {
            loop {
                match log_rx.recv().await {
                    Ok(bytes) => {
                        if state.lock().logs {
                            let frame = codec::encode_server(&ServerMessage::Push(
                                PushUpdate::LogLines(bytes.to_vec()),
                            ))
                            .unwrap_or_default();
                            if tx.send(WsMessage::Binary(frame.into())).is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        if state.lock().logs {
                            let marker = format!("--- log stream lagged, {n} lines skipped ---\n")
                                .into_bytes();
                            let frame = codec::encode_server(&ServerMessage::Push(
                                PushUpdate::LogLines(marker),
                            ))
                            .unwrap_or_default();
                            let _ = tx.send(WsMessage::Binary(frame.into()));
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Reader loop: decode client frames and dispatch.
    while let Some(Ok(msg)) = ws_receiver.next().await {
        let data = match msg {
            WsMessage::Binary(data) => data,
            WsMessage::Close(_) => break,
            _ => continue, // text / ping / pong: unused by this protocol
        };
        let msg = match codec::decode_client(&data) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "undecodable client frame");
                continue;
            }
        };
        match msg {
            ClientMessage::Request { id, req } => {
                let result = handle_request(&api, req, columns.as_deref()).await;
                let frame = codec::encode_server(&ServerMessage::Response { id, result })
                    .unwrap_or_else(|e| {
                        codec::encode_server(&ServerMessage::Response {
                            id,
                            result: Err(proto::ProtoError::internal(format!(
                                "failed to encode response: {e}"
                            ))),
                        })
                        .expect("error frame encoding cannot fail")
                    });
                if tx.send(WsMessage::Binary(frame.into())).is_err() {
                    break;
                }
            }
            ClientMessage::Subscribe { torrent } => match torrent {
                None => state.lock().session_level = true,
                Some(id) => {
                    state.lock().torrents.insert(id);
                }
            },
            ClientMessage::Unsubscribe { torrent } => match torrent {
                None => state.lock().session_level = false,
                Some(id) => {
                    state.lock().torrents.remove(&id);
                }
            },
            ClientMessage::SubscribeLogs(v) => state.lock().logs = v,
            ClientMessage::Ping => {
                let frame = codec::encode_server(&ServerMessage::Pong).unwrap_or_default();
                if tx.send(WsMessage::Binary(frame.into())).is_err() {
                    break;
                }
            }
        }
    }

    // Dropping `tx` ends the writer; the spawned push tasks exit when `tx`
    // sends start failing.
    drop(tx);
    let _ = writer.await;
}

/// The WS route plus the embedded web client, served on the daemon's own
/// port at `/` — the same port and path the classic React webui occupies
/// in the upstream `rqbit server`. `columns` (when present) provides
/// hook-declared custom columns: the sampler pushes their state and
/// list/details responses are decorated with their values.
pub fn router(
    api: Arc<Api>,
    line_broadcast: Option<LineBroadcast>,
    columns: Option<Arc<ColumnsProvider>>,
) -> axum::Router {
    use axum::routing::get;

    axum::Router::new()
        .route("/ws", get(ws_route))
        .fallback(serve_static)
        .with_state((api, line_broadcast, columns))
}

type RouterState = (
    Arc<Api>,
    Option<LineBroadcast>,
    Option<Arc<ColumnsProvider>>,
);

/// Serve the embedded web client under `/` (upstream-webui convention):
/// `/` (and `/index.html`) is the HTML, whose relative `./pkg/…` asset paths
/// resolve to `/pkg/…`. The same router is what the desktop app serves for
/// its own session (the bundled web view).
async fn serve_static(uri: axum::http::Uri) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let path = uri.path();
    match path {
        "/" | "/index.html" => {
            let html = if crate::embed::APP_BUILT {
                crate::embed::INDEX_HTML
            } else {
                crate::embed::NOT_BUILT_HTML
            };
            return (
                [(header::CONTENT_TYPE, crate::embed::CONTENT_TYPE_HTML)],
                html,
            )
                .into_response();
        }
        _ => {}
    }
    if let Some(p) = path.strip_prefix("/pkg/") {
        let (body, content_type) = match p {
            "web_gpui.js" => (crate::embed::APP_JS, crate::embed::CONTENT_TYPE_JS),
            "web_gpui_bg.wasm" => (crate::embed::APP_WASM, crate::embed::CONTENT_TYPE_WASM),
            _ => return (StatusCode::NOT_FOUND, "not found\n").into_response(),
        };
        return ([(header::CONTENT_TYPE, content_type)], body).into_response();
    }
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

async fn ws_route(
    axum::extract::State((api, line_broadcast, columns)): axum::extract::State<RouterState>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    upgrade.on_upgrade(move |socket| run_connection(socket, api, line_broadcast, columns))
}
