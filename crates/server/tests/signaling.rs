//! Drives a real server over WebSocket connections.

use std::time::Duration;

use flowerchat_protocol::{
    ClientMsg, IceServer, MAX_ROOM_SIZE, PeerId, PeerInfo, RoomInfo, ServerMsg,
};
use flowerchat_server::{Config, app};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const STUN: &str = "stun:test.invalid:3478";

async fn start_server() -> String {
    start_server_with(None).await
}

/// Starts a server that lets in only those invited in the file at `invites`,
/// or anyone if there is no such file.
async fn start_server_with(invites: Option<std::path::PathBuf>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        ice_servers: vec![IceServer {
            urls: vec![STUN.to_owned()],
            username: None,
            credential: None,
        }],
        releases: None,
        invites,
        rooms: vec!["lounge".into(), "games".into()],
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });
    format!("ws://{addr}/ws")
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn connect(url: &str) -> Self {
        let (ws, _) = connect_async(url).await.unwrap();
        Self { ws }
    }

    /// Connects, joins and returns the client with its id and the peers it
    /// was told about.
    async fn join(url: &str, room: &str, name: &str) -> (Self, PeerId, Vec<PeerInfo>) {
        let mut client = Self::connect(url).await;
        client
            .send(&ClientMsg::Join {
                room: room.into(),
                name: name.into(),
                invite: None,
            })
            .await;
        match client.recv().await {
            ServerMsg::Welcome { id, peers, .. } => (client, id, peers),
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    async fn send(&mut self, msg: &ClientMsg) {
        let json = serde_json::to_string(msg).unwrap();
        self.ws.send(Message::Text(json.into())).await.unwrap();
    }

    async fn recv(&mut self) -> ServerMsg {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("timed out waiting for a server message")
                .expect("connection closed")
                .unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// Asserts that nothing arrives for a short while.
    async fn expect_silence(&mut self) {
        let waited = tokio::time::timeout(Duration::from_millis(200), self.ws.next()).await;
        assert!(waited.is_err(), "unexpected message: {waited:?}");
    }
}

#[tokio::test]
async fn peers_discover_each_other_and_exchange_signals() {
    let url = start_server().await;

    let (mut alice, alice_id, seen_by_alice) = Client::join(&url, "lounge", "alice").await;
    assert!(seen_by_alice.is_empty());

    let (mut bob, bob_id, seen_by_bob) = Client::join(&url, "lounge", "bob").await;
    assert_eq!(
        seen_by_bob,
        vec![PeerInfo {
            id: alice_id,
            name: "alice".into()
        }]
    );
    assert_eq!(
        alice.recv().await,
        ServerMsg::PeerJoined {
            id: bob_id,
            name: "bob".into()
        }
    );

    let offer = json!({"description": {"type": "offer", "sdp": "v=0"}});
    bob.send(&ClientMsg::Signal {
        to: alice_id,
        data: offer.clone(),
    })
    .await;
    assert_eq!(
        alice.recv().await,
        ServerMsg::Signal {
            from: bob_id,
            data: offer
        }
    );

    drop(bob);
    assert_eq!(alice.recv().await, ServerMsg::PeerLeft { id: bob_id });
}

#[tokio::test]
async fn welcome_carries_the_configured_ice_servers() {
    let url = start_server().await;
    let mut client = Client::connect(&url).await;
    client
        .send(&ClientMsg::Join {
            room: "lounge".into(),
            name: "alice".into(),
            invite: None,
        })
        .await;
    match client.recv().await {
        ServerMsg::Welcome { ice_servers, .. } => {
            assert_eq!(ice_servers.len(), 1);
            assert_eq!(ice_servers[0].urls, vec![STUN.to_owned()]);
        }
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn signals_do_not_cross_rooms() {
    let url = start_server().await;
    let (mut alice, alice_id, _) = Client::join(&url, "lounge", "alice").await;
    let (mut eve, _, seen_by_eve) = Client::join(&url, "elsewhere", "eve").await;
    assert!(seen_by_eve.is_empty());

    eve.send(&ClientMsg::Signal {
        to: alice_id,
        data: json!({"candidate": "x"}),
    })
    .await;
    alice.expect_silence().await;
}

#[tokio::test]
async fn a_full_room_refuses_newcomers() {
    let url = start_server().await;
    let mut members = Vec::new();
    for i in 0..MAX_ROOM_SIZE {
        members.push(Client::join(&url, "lounge", &format!("peer{i}")).await);
    }

    let mut late = Client::connect(&url).await;
    late.send(&ClientMsg::Join {
        room: "lounge".into(),
        name: "late".into(),
        invite: None,
    })
    .await;
    assert_eq!(
        late.recv().await,
        ServerMsg::Error {
            message: "room is full".into()
        }
    );
}

#[tokio::test]
async fn a_server_with_invites_lets_in_only_those_invited() {
    let path =
        std::env::temp_dir().join(format!("flowerchat-ws-invites-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let token = flowerchat_server::invites::invite(&path, "anna", &["lounge".into()]).unwrap();
    let url = start_server_with(Some(path.clone())).await;

    let refused = |room: &'static str, invite: Option<String>| {
        let url = url.clone();
        async move {
            let mut client = Client::connect(&url).await;
            client
                .send(&ClientMsg::Join {
                    room: room.into(),
                    name: "anna".into(),
                    invite,
                })
                .await;
            match client.recv().await {
                ServerMsg::Error { message } => Some(message),
                ServerMsg::Welcome { .. } => None,
                other => panic!("expected a welcome or an error, got {other:?}"),
            }
        }
    };

    assert_eq!(
        refused("lounge", None).await.as_deref(),
        Some("this server needs an invite")
    );
    assert_eq!(
        refused("lounge", Some("made-up".into())).await.as_deref(),
        Some("this invite is not valid, or no longer")
    );
    assert_eq!(
        refused("games", Some(token.clone())).await.as_deref(),
        Some("this invite is not for this room")
    );
    assert_eq!(refused("lounge", Some(token.clone())).await, None);
    // A pasted link works as well as the token in it.
    let link = format!("https://chat.example.org/?invite={token}");
    assert_eq!(refused("lounge", Some(link)).await, None);

    // Taking the invite back needs no restart.
    assert!(flowerchat_server::invites::revoke(&path, "anna").unwrap());
    assert_eq!(
        refused("lounge", Some(token)).await.as_deref(),
        Some("this invite is not valid, or no longer")
    );
}

/// What a watching connection is told next, as room names with the names
/// of those in each.
async fn rooms(client: &mut Client) -> Vec<(String, Vec<String>)> {
    match client.recv().await {
        ServerMsg::Rooms { rooms } => rooms
            .into_iter()
            .map(|RoomInfo { name, peers }| {
                (name, peers.into_iter().map(|peer| peer.name).collect())
            })
            .collect(),
        other => panic!("expected the rooms, got {other:?}"),
    }
}

fn room(name: &str, peers: &[&str]) -> (String, Vec<String>) {
    (
        name.into(),
        peers.iter().map(|peer| (*peer).into()).collect(),
    )
}

#[tokio::test]
async fn a_watcher_is_told_who_is_in_which_room() {
    let url = start_server().await;
    let mut watcher = Client::connect(&url).await;
    watcher.send(&ClientMsg::Watch { invite: None }).await;
    assert_eq!(
        rooms(&mut watcher).await,
        [room("lounge", &[]), room("games", &[])]
    );

    let (alice, ..) = Client::join(&url, "games", "alice").await;
    assert_eq!(
        rooms(&mut watcher).await,
        [room("lounge", &[]), room("games", &["alice"])]
    );
    let (_bob, ..) = Client::join(&url, "games", "bob").await;
    assert_eq!(
        rooms(&mut watcher).await,
        [room("lounge", &[]), room("games", &["alice", "bob"])]
    );
    // A room nobody set up is there while someone is in it.
    let (carol, ..) = Client::join(&url, "attic", "carol").await;
    assert_eq!(
        rooms(&mut watcher).await,
        [
            room("lounge", &[]),
            room("games", &["alice", "bob"]),
            room("attic", &["carol"])
        ]
    );
    drop(carol);
    assert_eq!(
        rooms(&mut watcher).await,
        [room("lounge", &[]), room("games", &["alice", "bob"])]
    );
    drop(alice);
    assert_eq!(
        rooms(&mut watcher).await,
        [room("lounge", &[]), room("games", &["bob"])]
    );
}

#[tokio::test]
async fn a_watcher_sees_only_the_rooms_of_its_invite() {
    let path =
        std::env::temp_dir().join(format!("flowerchat-ws-watch-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let everywhere = flowerchat_server::invites::invite(&path, "anna", &[]).unwrap();
    let attic = flowerchat_server::invites::invite(&path, "bo", &["attic".into()]).unwrap();
    let url = start_server_with(Some(path)).await;

    let mut uninvited = Client::connect(&url).await;
    uninvited.send(&ClientMsg::Watch { invite: None }).await;
    assert_eq!(
        uninvited.recv().await,
        ServerMsg::Error {
            message: "this server needs an invite".into()
        }
    );

    let mut anna = Client::connect(&url).await;
    anna.send(&ClientMsg::Watch {
        invite: Some(everywhere),
    })
    .await;
    assert_eq!(
        rooms(&mut anna).await,
        [room("lounge", &[]), room("games", &[])]
    );

    let mut bo = Client::connect(&url).await;
    bo.send(&ClientMsg::Watch {
        invite: Some(attic),
    })
    .await;
    assert_eq!(rooms(&mut bo).await, [room("attic", &[])]);
}

#[tokio::test]
async fn a_connection_must_join_before_signaling() {
    let url = start_server().await;
    let mut client = Client::connect(&url).await;
    client
        .send(&ClientMsg::Signal {
            to: 1,
            data: json!({}),
        })
        .await;
    assert!(matches!(client.recv().await, ServerMsg::Error { .. }));
}

#[tokio::test]
async fn the_server_says_when_it_is_up() {
    use std::io::{Read, Write};

    let url = start_server().await;
    let address = url
        .trim_start_matches("ws://")
        .trim_end_matches("/ws")
        .to_owned();
    // What a container's health check does: a plain HTTP request.
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(b"GET /healthz HTTP/1.0\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    })
    .await
    .unwrap();
    assert!(reply.starts_with("HTTP/1.0 200"), "{reply}");
    assert!(reply.ends_with("ok"), "{reply}");
}
