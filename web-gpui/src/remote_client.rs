//! [`TorrentClient`] implementation for the browser: the `rqbit-protocol`
//! binary protocol (postcard frames) over a `web_sys::WebSocket` to
//! `app-daemon`, with automatic reconnect and request correlation.
//!
//! Everything runs on the single wasm thread; shared state is `Rc<RefCell>`.
//! One persistent pump task owns the WebSocket lifecycle: outgoing frames
//! are queued on an mpsc channel that survives reconnects (frames issued
//! while offline are flushed once the socket reopens), and incoming frames
//! are routed to either a pending-request oneshot or the push/log streams.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::stream::LocalBoxStream;
use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;

use rqbit_protocol as proto;
use rqbit_protocol::codec;
use rqbit_protocol::{
    ApiAddTorrent, ClientMessage, DhtStats, ProtoError, PushUpdate, Request, RequestId,
    ResponsePayload, ServerMessage, SessionStats, TorrentAction, TorrentDetails, TorrentId,
    TorrentList, TorrentStats,
};

use rqbit_ui::client::{AddTorrentRequest, ClientResult, TorrentClient};

const RECONNECT_DELAY_MS: i32 = 2000;

pub struct RemoteClient {
    url: String,
    next_id: Cell<RequestId>,
    /// Cached `Request::SessionInfo` answer (the daemon's download folder is
    /// fixed for its lifetime).
    default_folder: RefCell<Option<String>>,
    state: Rc<RefCell<State>>,
}

struct State {
    /// Outgoing frame queue; consumed by the pump and flushed whenever the
    /// socket is open. Frames sent while offline are delivered on reconnect.
    outgoing: Option<mpsc::UnboundedSender<Vec<u8>>>,
    pending: HashMap<RequestId, oneshot::Sender<ClientResult<ResponsePayload>>>,
    /// Created by `updates()`; the pump routes pushes into it.
    push_tx: Option<mpsc::UnboundedSender<PushUpdate>>,
    /// Created by `log_lines()`; the pump routes log lines into it.
    log_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// Whether the log subscription should be re-sent after a reconnect.
    logs_subscribed: bool,
    pump_started: bool,
}

impl RemoteClient {
    pub fn new(url: String) -> Self {
        Self {
            url,
            next_id: Cell::new(1),
            default_folder: RefCell::new(None),
            state: Rc::new(RefCell::new(State {
                outgoing: None,
                pending: HashMap::new(),
                push_tx: None,
                log_tx: None,
                logs_subscribed: false,
                pump_started: false,
            })),
        }
    }

    fn ensure_started(&self) {
        if self.state.borrow().pump_started {
            return;
        }
        let (tx, rx) = mpsc::unbounded::<Vec<u8>>();
        {
            let mut st = self.state.borrow_mut();
            st.outgoing = Some(tx);
            st.pump_started = true;
        }
        let url = self.url.clone();
        let state = self.state.clone();
        wasm_bindgen_futures::spawn_local(async move {
            pump_loop(url, state, rx).await;
        });
    }

    fn send_frame(&self, msg: &ClientMessage) {
        self.ensure_started();
        let frame = match codec::encode_client(msg) {
            Ok(frame) => frame,
            Err(e) => {
                web_sys::console::error_1(&format!("encode failed: {e}").into());
                return;
            }
        };
        if let Some(outgoing) = &self.state.borrow().outgoing {
            let _ = outgoing.unbounded_send(frame);
        }
    }

    /// Issue a request and await its correlated response. The request frame
    /// is queued even while offline; it is delivered on reconnect.
    fn request(
        &self,
        req: Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ClientResult<ResponsePayload>> + 'static>>
    {
        self.ensure_started();
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1).max(1));
        let (tx, rx) = oneshot::channel::<ClientResult<ResponsePayload>>();
        self.state.borrow_mut().pending.insert(id, tx);
        self.send_frame(&ClientMessage::Request { id, req });
        Box::pin(async move {
            match rx.await {
                Ok(result) => result,
                Err(_) => Err(ProtoError::internal("connection closed before response")),
            }
        })
    }
}

fn fail_pending(state: &Rc<RefCell<State>>, message: &str) {
    let mut st = state.borrow_mut();
    for (_, tx) in st.pending.drain() {
        let _ = tx.send(Err(ProtoError::internal(message)));
    }
}

fn unexpected(payload: ResponsePayload, expected: &'static str) -> ProtoError {
    let got = match payload {
        ResponsePayload::List(_) => "List",
        ResponsePayload::Details(_) => "Details",
        ResponsePayload::Stats(_) => "Stats",
        ResponsePayload::PeerStats(_) => "PeerStats",
        ResponsePayload::SessionStats(_) => "SessionStats",
        ResponsePayload::SessionInfo(_) => "SessionInfo",
        ResponsePayload::SessionSettings(_) => "SessionSettings",
        ResponsePayload::DhtStats(_) => "DhtStats",
        ResponsePayload::Added(_) => "Added",
        ResponsePayload::Empty => "Empty",
    };
    ProtoError::internal(format!(
        "unexpected response payload {got}, expected {expected}"
    ))
}

/// Route one decoded server frame to its waiter or push stream.
fn handle_frame(state: &Rc<RefCell<State>>, bytes: Vec<u8>) {
    let msg = match codec::decode_server(&bytes) {
        Ok(msg) => msg,
        Err(e) => {
            web_sys::console::warn_1(&format!("undecodable server frame: {e}").into());
            return;
        }
    };
    match msg {
        ServerMessage::Response { id, result } => {
            let tx = state.borrow_mut().pending.remove(&id);
            if let Some(tx) = tx {
                let _ = tx.send(result);
            }
        }
        ServerMessage::Push(push) => {
            let st = state.borrow_mut();
            if let PushUpdate::LogLines(lines) = push {
                if let Some(log_tx) = &st.log_tx {
                    let _ = log_tx.unbounded_send(lines);
                }
            } else if let Some(push_tx) = &st.push_tx {
                let _ = push_tx.unbounded_send(push);
            }
        }
        ServerMessage::Pong => {}
    }
}

/// WebSocket lifecycle: connect, route frames, reconnect with backoff.
async fn pump_loop(
    url: String,
    state: Rc<RefCell<State>>,
    mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    loop {
        if let Err(e) = connect_once(&url, &state, &mut out_rx).await {
            web_sys::console::error_1(
                &format!("websocket error, retrying in {RECONNECT_DELAY_MS}ms: {e:?}").into(),
            );
        }
        fail_pending(&state, "connection lost");
        let _ = sleep_ms(RECONNECT_DELAY_MS).await;
    }
}

async fn connect_once(
    url: &str,
    state: &Rc<RefCell<State>>,
    out_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<(), JsValue> {
    let ws = web_sys::WebSocket::new(url)?;
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // Cloneable channels: the open/close signals each feed multiple JS
    // event callbacks (futures' oneshot senders are not Clone).
    let (open_tx, mut open_rx) = mpsc::unbounded::<Result<(), String>>();
    let (close_tx, close_rx) = mpsc::unbounded::<()>();

    let on_open = {
        let open_tx = open_tx.clone();
        Closure::<dyn FnMut()>::new(move || {
            let _ = open_tx.unbounded_send(Ok(()));
        })
    };
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));

    let on_error = {
        let open_tx = open_tx.clone();
        let close_tx = close_tx.clone();
        Closure::<dyn FnMut(js_sys::Object)>::new(move |_e: js_sys::Object| {
            let _ = open_tx.unbounded_send(Err("websocket failed to connect".to_string()));
            let _ = close_tx.unbounded_send(());
        })
    };
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    let on_close = {
        let close_tx = close_tx.clone();
        Closure::<dyn FnMut()>::new(move || {
            let _ = close_tx.unbounded_send(());
        })
    };
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    let on_message = {
        let state = state.clone();
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                handle_frame(&state, Uint8Array::new(&buf).to_vec());
            }
        })
    };
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    // Wait for the socket to open (or fail).
    match open_rx.next().await {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            clear_handlers(&ws);
            return Err(JsValue::from_str(&e));
        }
        None => {
            clear_handlers(&ws);
            return Err(JsValue::from_str("connect aborted"));
        }
    }

    // Connected: (re-)establish subscriptions. Client request frames queued
    // on `out_rx` while offline are flushed by the loop below.
    {
        let logs_subscribed = state.borrow().logs_subscribed;
        if logs_subscribed {
            let _ = ws.send_with_u8_array(
                &codec::encode_client(&ClientMessage::SubscribeLogs(true)).unwrap_or_default(),
            );
        }
        let _ = ws.send_with_u8_array(
            &codec::encode_client(&ClientMessage::Subscribe { torrent: None }).unwrap_or_default(),
        );
    }

    let mut out_rx_fused = out_rx.fuse();
    let mut close_rx = close_rx.fuse();
    loop {
        futures::select! {
            frame = out_rx_fused.next() => match frame {
                Some(frame) => {
                    if ws.send_with_u8_array(&frame).is_err() {
                        break;
                    }
                }
                None => break, // client dropped; pump ends after this connection
            },
            // Any close/error event ends this connection; the pump reconnects.
            _ = close_rx.select_next_some() => break,
        }
    }

    clear_handlers(&ws);
    let _ = ws.close();
    Ok(())
}

fn clear_handlers(ws: &web_sys::WebSocket) {
    ws.set_onopen(None);
    ws.set_onerror(None);
    ws.set_onclose(None);
    ws.set_onmessage(None);
}

fn sleep_ms(ms: i32) -> wasm_bindgen_futures::JsFuture {
    let window = web_sys::window().expect("no window");
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    wasm_bindgen_futures::JsFuture::from(promise)
}

#[async_trait(?Send)]
impl TorrentClient for RemoteClient {
    async fn list(&self, with_stats: bool) -> ClientResult<TorrentList> {
        match self.request(Request::List { with_stats }).await? {
            ResponsePayload::List(list) => Ok(*list),
            other => Err(unexpected(other, "List")),
        }
    }

    async fn details(&self, torrent: &TorrentId) -> ClientResult<TorrentDetails> {
        let torrent = torrent.clone();
        match self.request(Request::Details { torrent }).await? {
            ResponsePayload::Details(details) => Ok(*details),
            other => Err(unexpected(other, "Details")),
        }
    }

    async fn stats(&self, torrent: &TorrentId) -> ClientResult<TorrentStats> {
        let torrent = torrent.clone();
        match self.request(Request::Stats { torrent }).await? {
            ResponsePayload::Stats(stats) => Ok(*stats),
            other => Err(unexpected(other, "Stats")),
        }
    }

    async fn peer_stats(&self, torrent: &TorrentId) -> ClientResult<proto::PeerStatsSnapshot> {
        let torrent = torrent.clone();
        match self.request(Request::PeerStats { torrent }).await? {
            ResponsePayload::PeerStats(peers) => Ok(peers),
            other => Err(unexpected(other, "PeerStats")),
        }
    }

    async fn session_stats(&self) -> ClientResult<SessionStats> {
        match self.request(Request::SessionStats).await? {
            ResponsePayload::SessionStats(stats) => Ok(*stats),
            other => Err(unexpected(other, "SessionStats")),
        }
    }

    async fn session_default_folder(&self) -> ClientResult<Option<String>> {
        // The daemon's download folder is fixed for its lifetime; cache it so
        // the per-second table refresh doesn't re-ask.
        if let Some(cached) = self.default_folder.borrow().clone() {
            return Ok(Some(cached));
        }
        match self.request(Request::SessionInfo).await? {
            ResponsePayload::SessionInfo(info) => {
                let folder = info.default_download_folder;
                *self.default_folder.borrow_mut() = Some(folder.clone());
                Ok(Some(folder))
            }
            other => Err(unexpected(other, "SessionInfo")),
        }
    }

    async fn dht_stats(&self) -> ClientResult<DhtStats> {
        match self.request(Request::DhtStats).await? {
            ResponsePayload::DhtStats(stats) => Ok(stats),
            other => Err(unexpected(other, "DhtStats")),
        }
    }

    async fn action(&self, torrent: &TorrentId, action: TorrentAction) -> ClientResult<()> {
        let torrent = torrent.clone();
        match self.request(Request::Action { torrent, action }).await? {
            ResponsePayload::Empty => Ok(()),
            other => Err(unexpected(other, "Empty")),
        }
    }

    async fn add_torrent(&self, req: &AddTorrentRequest) -> ClientResult<ApiAddTorrent> {
        let source = req.source.to_wire()?;
        match self
            .request(Request::AddTorrent {
                source,
                only_files: req.only_files.clone(),
                overwrite: req.overwrite,
                paused: req.paused,
                list_only: req.list_only,
            })
            .await?
        {
            ResponsePayload::Added(added) => Ok(*added),
            other => Err(unexpected(other, "Added")),
        }
    }

    async fn get_session_settings(&self) -> ClientResult<proto::SessionSettings> {
        match self.request(Request::GetSessionSettings).await? {
            ResponsePayload::SessionSettings(settings) => Ok(*settings),
            other => Err(unexpected(other, "SessionSettings")),
        }
    }

    async fn set_session_settings(&self, settings: &proto::SessionSettings) -> ClientResult<()> {
        match self
            .request(Request::SetSessionSettings(Box::new(settings.clone())))
            .await?
        {
            ResponsePayload::Empty => Ok(()),
            other => Err(unexpected(other, "Empty")),
        }
    }

    async fn set_rust_log(&self, value: String) -> ClientResult<()> {
        match self.request(Request::SetRustLog(value)).await? {
            ResponsePayload::Empty => Ok(()),
            other => Err(unexpected(other, "Empty")),
        }
    }

    fn log_lines(&self) -> ClientResult<LocalBoxStream<'static, Vec<u8>>> {
        self.ensure_started();
        // A fresh channel per call: the previous consumer's stream simply
        // ends when its sender is replaced.
        let (tx, rx) = mpsc::unbounded::<Vec<u8>>();
        self.state.borrow_mut().log_tx = Some(tx);
        self.state.borrow_mut().logs_subscribed = true;
        self.send_frame(&ClientMessage::SubscribeLogs(true));
        Ok(Box::pin(rx.map(|lines| lines)))
    }

    fn is_push(&self) -> bool {
        true
    }

    fn updates(&self) -> LocalBoxStream<'static, PushUpdate> {
        self.ensure_started();
        let (tx, rx) = mpsc::unbounded::<PushUpdate>();
        let mut st = self.state.borrow_mut();
        if st.push_tx.is_none() {
            st.push_tx = Some(tx);
            Box::pin(rx.map(|push| push))
        } else {
            // A second consumer is not supported; drops the fresh channel.
            Box::pin(futures::stream::empty())
        }
    }
}
