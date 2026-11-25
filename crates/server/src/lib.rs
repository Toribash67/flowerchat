//! flowerchat signaling server.
//!
//! Keeps track of who is in which room and relays WebRTC signaling between
//! peers. Media never passes through here.
//!
//! It also serves the web page: a smaller flowerchat that talks and chats
//! from the browser, and links to the installers of the latest release.

mod releases;

pub use releases::ReleasesConfig;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use flowerchat_protocol::{
    ClientMsg, IceServer, MAX_NAME_LEN, MAX_ROOM_SIZE, PeerId, PeerInfo, ServerMsg,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use releases::{Lookup, Releases};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// SDP offers are a few kilobytes; anything near this limit is not signaling.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// How long a new connection has to send its `join`.
const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Keeps reverse proxies from closing idle connections, and lets us notice
/// clients that vanished without closing the socket.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A live client answers every ping, so silence this long means it is gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Messages queued per peer before new ones are dropped.
const OUTBOX_CAPACITY: usize = 256;

pub struct Config {
    /// Handed to every client in its `welcome`.
    pub ice_servers: Vec<IceServer>,
    /// Where the page finds installers to offer. Without it, it offers none.
    pub releases: Option<ReleasesConfig>,
}

pub fn app(config: Config) -> Router {
    let state = AppState(Arc::new(Shared {
        rooms: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        ice_servers: config.ice_servers,
        releases: config.releases.map(Releases::new),
    }));
    Router::new()
        .route("/", get(|| page("text/html; charset=utf-8", PAGE)))
        .route("/style.css", get(|| page("text/css; charset=utf-8", STYLE)))
        .route(
            "/app.js",
            get(|| page("text/javascript; charset=utf-8", SCRIPT)),
        )
        .route("/icon.png", get(|| page("image/png", ICON)))
        .route("/api/release", get(latest_release))
        .route("/download/{file}", get(download))
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

const PAGE: &[u8] = include_bytes!("../static/index.html");
const STYLE: &[u8] = include_bytes!("../static/style.css");
const SCRIPT: &[u8] = include_bytes!("../static/app.js");
/// The app's own icon, so that the page's tab looks like the app.
const ICON: &[u8] = include_bytes!("../../app/assets/icon.png");

async fn page(content_type: &'static str, body: &'static [u8]) -> Response {
    // Checked again on every load, so that a new version of the page
    // never meets an old version of its script.
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// The latest release and the installers it has, for the page to list.
async fn latest_release(State(state): State<AppState>) -> Response {
    let Some(releases) = &state.0.releases else {
        let body = json!({ "error": "This server offers no downloads." });
        return (StatusCode::NOT_FOUND, Json(body)).into_response();
    };
    match releases.latest().await {
        Lookup::Found(release) => Json(json!({ "release": release })).into_response(),
        Lookup::None => Json(json!({ "release": null })).into_response(),
        Lookup::Failed(err) => {
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": err }))).into_response()
        }
    }
}

/// Sends the browser to an installer of the latest release, named by its
/// file name or by its platform, such as `/download/windows`.
async fn download(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    match &state.0.releases {
        Some(releases) => releases.download(&file).await,
        None => (StatusCode::NOT_FOUND, "This server offers no downloads.").into_response(),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

struct Peer {
    name: String,
    outbox: mpsc::Sender<ServerMsg>,
}

type Room = HashMap<PeerId, Peer>;

struct Shared {
    rooms: Mutex<HashMap<String, Room>>,
    next_id: AtomicU64,
    ice_servers: Vec<IceServer>,
    releases: Option<Releases>,
}

#[derive(Clone)]
struct AppState(Arc<Shared>);

impl AppState {
    /// Adds a peer to a room and queues its `welcome`.
    fn join(
        &self,
        room_name: &str,
        name: String,
        outbox: mpsc::Sender<ServerMsg>,
    ) -> Result<PeerId, &'static str> {
        let mut rooms = self.0.rooms.lock().unwrap();
        if rooms
            .get(room_name)
            .is_some_and(|r| r.len() >= MAX_ROOM_SIZE)
        {
            return Err("room is full");
        }
        let room = rooms.entry(room_name.to_owned()).or_default();
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);

        let peers = room
            .iter()
            .map(|(&id, peer)| PeerInfo {
                id,
                name: peer.name.clone(),
            })
            .collect();
        for (&other, peer) in room.iter() {
            deliver(
                other,
                &peer.outbox,
                ServerMsg::PeerJoined {
                    id,
                    name: name.clone(),
                },
            );
        }
        deliver(
            id,
            &outbox,
            ServerMsg::Welcome {
                id,
                peers,
                ice_servers: self.0.ice_servers.clone(),
            },
        );
        room.insert(id, Peer { name, outbox });
        Ok(id)
    }

    fn relay(&self, room_name: &str, from: PeerId, to: PeerId, data: serde_json::Value) {
        let rooms = self.0.rooms.lock().unwrap();
        // The target may have just left; that is not an error.
        if let Some(peer) = rooms.get(room_name).and_then(|room| room.get(&to)) {
            deliver(to, &peer.outbox, ServerMsg::Signal { from, data });
        }
    }

    fn leave(&self, room_name: &str, id: PeerId) {
        let mut rooms = self.0.rooms.lock().unwrap();
        let Some(room) = rooms.get_mut(room_name) else {
            return;
        };
        room.remove(&id);
        if room.is_empty() {
            rooms.remove(room_name);
            return;
        }
        for (&other, peer) in room.iter() {
            deliver(other, &peer.outbox, ServerMsg::PeerLeft { id });
        }
    }
}

fn deliver(to: PeerId, outbox: &mpsc::Sender<ServerMsg>, msg: ServerMsg) {
    if outbox.try_send(msg).is_err() {
        warn!(peer = to, "outbox full or closed, dropping message");
    }
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (sink, mut stream) = socket.split();

    let joined = tokio::time::timeout(JOIN_TIMEOUT, next_client_msg(&mut stream)).await;
    let (room, name) = match joined {
        Ok(Some(ClientMsg::Join { room, name })) => (room, name),
        Ok(None) => return,
        _ => return reject(sink, "expected a join message").await,
    };
    if !valid_name(&room) || !valid_name(&name) {
        return reject(sink, "room and name must be 1 to 64 bytes").await;
    }

    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    let id = match state.join(&room, name.clone(), outbox) {
        Ok(id) => id,
        Err(reason) => return reject(sink, reason).await,
    };
    info!(peer = id, %name, %room, "joined");

    let mut writer = tokio::spawn(write_loop(sink, inbox));
    loop {
        tokio::select! {
            msg = next_client_msg(&mut stream) => match msg {
                Some(ClientMsg::Signal { to, data }) => state.relay(&room, id, to, data),
                Some(ClientMsg::Join { .. }) => debug!(peer = id, "ignoring repeated join"),
                None => break,
            },
            // The socket stopped accepting writes.
            _ = &mut writer => break,
        }
    }

    // Removing the peer drops its outbox sender, which ends the writer.
    state.leave(&room, id);
    info!(peer = id, %room, "left");
}

/// Waits for the next well-formed client message. Returns `None` once the
/// connection is closed, broken or idle for too long.
async fn next_client_msg(stream: &mut SplitStream<WebSocket>) -> Option<ClientMsg> {
    loop {
        let frame = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
        match frame {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
                Ok(msg) => return Some(msg),
                // Skipped rather than fatal so that newer clients can send
                // message types this server does not know yet.
                Err(err) => debug!(%err, "ignoring unrecognised message"),
            },
            // Pings, pongs and binary frames only count as signs of life.
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
            Ok(Some(Ok(_))) => {}
        }
    }
}

async fn write_loop(mut sink: SplitSink<WebSocket, Message>, mut inbox: mpsc::Receiver<ServerMsg>) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // the first tick fires immediately
    loop {
        let frame = tokio::select! {
            msg = inbox.recv() => match msg {
                Some(msg) => encode(&msg),
                None => break,
            },
            _ = ping.tick() => Message::Ping(Default::default()),
        };
        if sink.send(frame).await.is_err() {
            return;
        }
    }
    let _ = sink.close().await;
}

async fn reject(mut sink: SplitSink<WebSocket, Message>, reason: &str) {
    let msg = ServerMsg::Error {
        message: reason.to_owned(),
    };
    let _ = sink.send(encode(&msg)).await;
    let _ = sink.close().await;
}

fn encode(msg: &ServerMsg) -> Message {
    let json = serde_json::to_string(msg).expect("server messages always serialize");
    Message::Text(json.into())
}

fn valid_name(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= MAX_NAME_LEN
}
