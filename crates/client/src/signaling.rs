//! Client side of the signaling connection.

use anyhow::{Context, Result, anyhow, bail};
use flowerchat_protocol::{ClientMsg, IceServer, PeerId, PeerInfo, RoomInfo, ServerMsg};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::debug;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Sender(SplitSink<Socket, Message>);
pub struct Receiver(SplitStream<Socket>);

/// What the server told us when we joined.
pub struct Welcome {
    pub id: PeerId,
    pub peers: Vec<PeerInfo>,
    pub ice_servers: Vec<IceServer>,
}

pub async fn join(
    url: &str,
    room: &str,
    name: &str,
    invite: Option<&str>,
) -> Result<(Welcome, Sender, Receiver)> {
    let (socket, _) = connect_async(url)
        .await
        .with_context(|| format!("cannot reach the signaling server at {url}"))?;
    let (sink, stream) = socket.split();
    let mut sender = Sender(sink);
    let mut receiver = Receiver(stream);

    sender
        .send(&ClientMsg::Join {
            room: room.to_owned(),
            name: name.to_owned(),
            invite: invite.map(str::to_owned),
        })
        .await?;
    match receiver.next().await? {
        Some(ServerMsg::Welcome {
            id,
            peers,
            ice_servers,
        }) => Ok((
            Welcome {
                id,
                peers,
                ice_servers,
            },
            sender,
            receiver,
        )),
        Some(ServerMsg::Error { message }) => bail!("the server refused the join: {message}"),
        Some(other) => bail!("expected a welcome from the server, got {other:?}"),
        None => bail!("the server closed the connection before welcoming us"),
    }
}

/// Asks the server which rooms there are and who is in them, and hands
/// each answer to `on_rooms`: one straight away, and one whenever someone
/// joins or leaves a room. Returns, with the reason, once the connection
/// ends. A server from before rooms were listed gives an error after some
/// seconds of saying nothing.
pub async fn watch(
    url: &str,
    invite: Option<&str>,
    mut on_rooms: impl FnMut(Vec<RoomInfo>),
) -> Result<()> {
    let (socket, _) = connect_async(url)
        .await
        .with_context(|| format!("cannot reach the signaling server at {url}"))?;
    let (sink, stream) = socket.split();
    let mut sender = Sender(sink);
    let mut receiver = Receiver(stream);
    sender
        .send(&ClientMsg::Watch {
            invite: invite.map(str::to_owned),
        })
        .await?;
    loop {
        match receiver.next().await? {
            Some(ServerMsg::Rooms { rooms }) => on_rooms(rooms),
            Some(ServerMsg::Error { message }) => bail!("the server said: {message}"),
            Some(_) => {}
            None => bail!("the signaling server disconnected"),
        }
    }
}

impl Sender {
    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let json = serde_json::to_string(msg)?;
        self.0
            .send(Message::Text(json.into()))
            .await
            .context("lost the connection to the signaling server")
    }
}

impl Receiver {
    /// The next message from the server, or `None` once it disconnects.
    pub async fn next(&mut self) -> Result<Option<ServerMsg>> {
        while let Some(frame) = self.0.next().await {
            match frame.map_err(|err| anyhow!("signaling connection failed: {err}"))? {
                Message::Text(text) => match serde_json::from_str(&text) {
                    Ok(msg) => return Ok(Some(msg)),
                    // A newer server may send message types we do not know.
                    Err(err) => debug!(%err, "ignoring unrecognised server message"),
                },
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}
