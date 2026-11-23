//! Signaling messages exchanged between flowerchat clients and the server.
//!
//! The server only handles room membership and relays `signal` payloads
//! between peers in the same room. The payload itself (SDP descriptions and
//! ICE candidates) is opaque to the server.
//!
//! Every message is a JSON object with a `type` field, for example
//! `{"type":"join","room":"lounge","name":"martin"}`.

use serde::{Deserialize, Serialize};

/// Identifies a peer for the lifetime of one connection.
pub type PeerId = u64;

/// Most peers a room accepts. A full mesh grows quadratically, so this is a
/// hard cap rather than a tunable.
pub const MAX_ROOM_SIZE: usize = 8;

/// Longest accepted room or display name, in bytes.
pub const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientMsg {
    /// Must be the first message on a connection.
    Join { room: String, name: String },
    /// Relay `data` to another peer in the same room.
    Signal { to: PeerId, data: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ServerMsg {
    /// Reply to a successful `join`.
    Welcome {
        id: PeerId,
        /// Peers already in the room.
        peers: Vec<PeerInfo>,
        ice_servers: Vec<IceServer>,
    },
    PeerJoined {
        id: PeerId,
        name: String,
    },
    PeerLeft {
        id: PeerId,
    },
    Signal {
        from: PeerId,
        data: serde_json::Value,
    },
    /// Sent before the server closes the connection.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
}

/// Same shape as the browser's `RTCIceServer`, so clients can pass it through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_messages_use_kebab_case_tags() {
        let msg: ClientMsg =
            serde_json::from_value(json!({"type": "join", "room": "lounge", "name": "martin"}))
                .unwrap();
        assert_eq!(
            msg,
            ClientMsg::Join {
                room: "lounge".into(),
                name: "martin".into()
            }
        );
    }

    #[test]
    fn server_messages_match_what_browsers_expect() {
        let welcome = ServerMsg::Welcome {
            id: 2,
            peers: vec![PeerInfo {
                id: 1,
                name: "a".into(),
            }],
            ice_servers: vec![IceServer {
                urls: vec!["stun:example.org:3478".into()],
                username: None,
                credential: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&welcome).unwrap(),
            json!({
                "type": "welcome",
                "id": 2,
                "peers": [{"id": 1, "name": "a"}],
                "iceServers": [{"urls": ["stun:example.org:3478"]}],
            })
        );
        assert_eq!(
            serde_json::to_value(ServerMsg::PeerJoined {
                id: 3,
                name: "b".into()
            })
            .unwrap(),
            json!({"type": "peer-joined", "id": 3, "name": "b"})
        );
    }
}
