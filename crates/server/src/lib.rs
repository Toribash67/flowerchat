//! flowerchat signaling server.
//!
//! Keeps track of who is in which room and relays WebRTC signaling between
//! peers. Media never passes through here.
//!
//! It also serves the web page: a smaller flowerchat that talks and chats
//! from the browser, and links to the installers of the latest release.

pub mod invites;
mod releases;

pub use releases::ReleasesConfig;

use std::collections::HashMap;
use std::path::PathBuf;
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
    ClientMsg, IceServer, MAX_NAME_LEN, MAX_ROOM_SIZE, PeerId, PeerInfo, RoomInfo, ServerMsg,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use invites::Invites;
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
    /// The file of invites. With it, only those invited may join a room;
    /// without it, anyone who reaches the server may.
    pub invites: Option<PathBuf>,
    /// The rooms that are always there to join, in the order clients list
    /// them. Any other room exists for as long as someone is in it.
    pub rooms: Vec<String>,
}

pub fn app(config: Config) -> Router {
    let state = AppState(Arc::new(Shared {
        state: Mutex::new(Rooms::default()),
        listed: config.rooms,
        next_id: AtomicU64::new(1),
        ice_servers: config.ice_servers,
        releases: config.releases.map(Releases::new),
        invites: config.invites.map(Invites::new),
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
        .route("/api/server", get(about))
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

/// What a client needs to know before it joins: whether to ask for an
/// invite.
async fn about(State(state): State<AppState>) -> Response {
    Json(json!({ "invites": state.0.invites.is_some() })).into_response()
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

/// A connection that is told which rooms there are and who is in them.
struct Watcher {
    outbox: mpsc::Sender<ServerMsg>,
    /// The rooms its invite is for, which are all it is told about. Empty
    /// means every room.
    only: Vec<String>,
}

#[derive(Default)]
struct Rooms {
    rooms: HashMap<String, Room>,
    watchers: HashMap<PeerId, Watcher>,
}

struct Shared {
    state: Mutex<Rooms>,
    /// The rooms that are there even with nobody in them.
    listed: Vec<String>,
    next_id: AtomicU64,
    ice_servers: Vec<IceServer>,
    releases: Option<Releases>,
    invites: Option<Invites>,
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
        let mut state = self.0.state.lock().unwrap();
        let rooms = &mut state.rooms;
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
        self.announce(&state);
        Ok(id)
    }

    /// Starts telling a connection about the rooms, beginning now.
    fn watch(&self, only: Vec<String>, outbox: mpsc::Sender<ServerMsg>) -> PeerId {
        let mut state = self.0.state.lock().unwrap();
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let rooms = self.listing(&state, &only);
        deliver(id, &outbox, ServerMsg::Rooms { rooms });
        state.watchers.insert(id, Watcher { outbox, only });
        id
    }

    fn unwatch(&self, id: PeerId) {
        self.0.state.lock().unwrap().watchers.remove(&id);
    }

    /// Tells everyone watching how the rooms stand.
    fn announce(&self, state: &Rooms) {
        for (&id, watcher) in &state.watchers {
            let rooms = self.listing(state, &watcher.only);
            deliver(id, &watcher.outbox, ServerMsg::Rooms { rooms });
        }
    }

    /// The rooms to show someone whose invite is for `only`, or for every
    /// room if that is empty: the rooms that are always there, then the
    /// invite's own, then whichever others have someone in them.
    fn listing(&self, state: &Rooms, only: &[String]) -> Vec<RoomInfo> {
        let mut names: Vec<&String> = self.0.listed.iter().collect();
        names.extend(only.iter().filter(|name| !self.0.listed.contains(name)));
        let mut others: Vec<&String> = state
            .rooms
            .keys()
            .filter(|name| !names.contains(name))
            .collect();
        others.sort();
        names.extend(others);
        names
            .into_iter()
            .filter(|name| only.is_empty() || only.contains(name))
            .map(|name| {
                let mut peers: Vec<PeerInfo> = state
                    .rooms
                    .get(name)
                    .into_iter()
                    .flatten()
                    .map(|(&id, peer)| PeerInfo {
                        id,
                        name: peer.name.clone(),
                    })
                    .collect();
                // Ids are handed out in order, so this is the order of arrival.
                peers.sort_by_key(|peer| peer.id);
                RoomInfo {
                    name: name.clone(),
                    peers,
                }
            })
            .collect()
    }

    fn relay(&self, room_name: &str, from: PeerId, to: PeerId, data: serde_json::Value) {
        let state = self.0.state.lock().unwrap();
        // The target may have just left; that is not an error.
        if let Some(peer) = state.rooms.get(room_name).and_then(|room| room.get(&to)) {
            deliver(to, &peer.outbox, ServerMsg::Signal { from, data });
        }
    }

    fn leave(&self, room_name: &str, id: PeerId) {
        let mut state = self.0.state.lock().unwrap();
        let Some(room) = state.rooms.get_mut(room_name) else {
            return;
        };
        room.remove(&id);
        if room.is_empty() {
            state.rooms.remove(room_name);
        } else {
            for (&other, peer) in room.iter() {
                deliver(other, &peer.outbox, ServerMsg::PeerLeft { id });
            }
        }
        self.announce(&state);
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
    let (room, name, invite) = match joined {
        Ok(Some(ClientMsg::Join { room, name, invite })) => (room, name, invite),
        Ok(Some(ClientMsg::Watch { invite })) => return watch(sink, stream, state, invite).await,
        Ok(None) => return,
        _ => return reject(sink, "expected a join message").await,
    };
    if !valid_name(&room) || !valid_name(&name) {
        return reject(sink, "room and name must be 1 to 64 bytes").await;
    }
    // On a server that invites, whose invite this is. It is for the log:
    // the others in the room see the name the peer chose.
    let invited = match &state.0.invites {
        Some(invites) => match invites.check(invite.as_deref(), &room) {
            Ok(invited) => Some(invited),
            Err(refusal) => {
                info!(%name, %room, ?refusal, "turned away");
                return reject(sink, refusal.message()).await;
            }
        },
        None => None,
    };

    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    let id = match state.join(&room, name.clone(), outbox) {
        Ok(id) => id,
        Err(reason) => return reject(sink, reason).await,
    };
    info!(peer = id, %name, %room, invite = invited.as_deref(), "joined");

    let mut writer = tokio::spawn(write_loop(sink, inbox));
    loop {
        tokio::select! {
            msg = next_client_msg(&mut stream) => match msg {
                Some(ClientMsg::Signal { to, data }) => state.relay(&room, id, to, data),
                Some(ClientMsg::Join { .. } | ClientMsg::Watch { .. }) => {
                    debug!(peer = id, "ignoring a second join")
                }
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

/// Keeps a connection told about the rooms until it closes.
async fn watch(
    sink: SplitSink<WebSocket, Message>,
    mut stream: SplitStream<WebSocket>,
    state: AppState,
    invite: Option<String>,
) {
    let only = match &state.0.invites {
        Some(invites) => match invites.find(invite.as_deref()) {
            Ok(invite) => invite.rooms,
            Err(refusal) => return reject(sink, refusal.message()).await,
        },
        None => Vec::new(),
    };
    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    let id = state.watch(only, outbox);
    let mut writer = tokio::spawn(write_loop(sink, inbox));
    loop {
        tokio::select! {
            // It has nothing to say, but its pongs show that it is there.
            msg = next_client_msg(&mut stream) => if msg.is_none() { break },
            _ = &mut writer => break,
        }
    }
    state.unwatch(id);
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
