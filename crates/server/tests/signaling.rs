//! Drives a real server over WebSocket connections.

use std::time::Duration;

use flowerchat_protocol::{ClientMsg, IceServer, MAX_ROOM_SIZE, PeerId, PeerInfo, ServerMsg};
use flowerchat_server::{Config, app};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const STUN: &str = "stun:test.invalid:3478";

async fn start_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        ice_servers: vec![IceServer {
            urls: vec![STUN.to_owned()],
            username: None,
            credential: None,
        }],
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
