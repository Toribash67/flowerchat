//! One stay in a room, from joining to leaving.
//!
//! A user interface starts [`run`], sends it [`Command`]s and draws what the
//! [`Event`]s describe. Everything else (signaling, calls, audio) stays
//! inside.

use std::time::Duration;

use anyhow::{Result, anyhow};
use flowerchat_protocol::{ClientMsg, PeerId, ServerMsg};
use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::call::{Call, CallConfig, CallEvent};
pub use crate::call::{ConnectionState, PeerStats, Route};
use crate::ice::IceConfig;
use crate::signaling;

/// How often connection statistics are refreshed.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// WebSocket address of the signaling server.
    pub server: String,
    pub room: String,
    pub name: String,
    /// PulseAudio/PipeWire source name, or the default source.
    pub input: Option<String>,
    /// PulseAudio/PipeWire sink name, or the default sink.
    pub output: Option<String>,
    /// Size of the receive buffer that absorbs network jitter.
    pub jitter_ms: u32,
    /// Send the microphone without echo cancellation, noise suppression or
    /// gain control.
    pub raw_mic: bool,
    /// Send measurement clicks instead of the microphone.
    pub test_tone: bool,
    /// Time incoming clicks instead of playing audio.
    pub measure: bool,
}

#[derive(Debug, Clone)]
pub enum Command {
    SetMuted(bool),
    SetDeafened(bool),
    /// Playback gain for one peer, 1.0 being unchanged.
    SetPeerVolume {
        id: PeerId,
        volume: f64,
    },
    SendChat(String),
    Leave,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// We are in the room, with this id.
    Joined {
        id: PeerId,
    },
    PeerJoined {
        id: PeerId,
        name: String,
    },
    PeerLeft {
        id: PeerId,
    },
    PeerState {
        id: PeerId,
        state: ConnectionState,
    },
    PeerMuted {
        id: PeerId,
        muted: bool,
    },
    /// Loudness of what a peer is sending, in dB below full scale.
    PeerLevel {
        id: PeerId,
        db: f32,
    },
    /// Loudness of what we are sending, in dB below full scale.
    MicLevel {
        db: f32,
    },
    PeerStats {
        id: PeerId,
        stats: PeerStats,
    },
    Chat {
        from: PeerId,
        text: String,
    },
    /// A measurement click arrived this many nanoseconds after it was sent.
    ClickDelay(u64),
}

/// Joins the room and stays until told to leave. Returns an error if the
/// join fails or the session breaks down.
pub async fn run(
    config: SessionConfig,
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) -> Result<()> {
    let (welcome, mut to_server, mut from_server) =
        signaling::join(&config.server, &config.room, &config.name).await?;
    let my_id = welcome.id;

    let (call_events_tx, mut call_events) = unbounded_channel();
    let call = Call::new(
        CallConfig {
            input: config.input,
            output: config.output,
            jitter_ms: config.jitter_ms,
            raw_mic: config.raw_mic,
            test_tone: config.test_tone,
            measure: config.measure,
            ice: IceConfig::from_servers(&welcome.ice_servers),
        },
        call_events_tx,
    )?;
    let mut bus = call.bus().stream();
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);

    let _ = events.send(Event::Joined { id: my_id });
    // The side with the lower id makes the offer. Ids only grow, so that is
    // always whoever was in the room first.
    let add_peer = |id: PeerId, name: String| {
        if let Err(err) = call.add_peer(id, &name, my_id < id) {
            tracing::warn!(peer = id, "cannot connect: {err:#}");
        }
        let _ = events.send(Event::PeerJoined { id, name });
    };
    for peer in welcome.peers {
        add_peer(peer.id, peer.name);
    }

    let outcome = loop {
        tokio::select! {
            msg = from_server.next() => match msg {
                Ok(Some(ServerMsg::PeerJoined { id, name })) => add_peer(id, name),
                Ok(Some(ServerMsg::PeerLeft { id })) => {
                    call.remove_peer(id);
                    let _ = events.send(Event::PeerLeft { id });
                }
                Ok(Some(ServerMsg::Signal { from, data })) => {
                    if let Err(err) = call.on_signal(from, &data) {
                        tracing::warn!(peer = from, "bad signaling: {err:#}");
                    }
                }
                Ok(Some(ServerMsg::Error { message })) => break Err(anyhow!("server: {message}")),
                Ok(Some(ServerMsg::Welcome { .. })) => {}
                Ok(None) => break Err(anyhow!("the signaling server disconnected")),
                Err(err) => break Err(err),
            },
            Some(event) = call_events.recv() => {
                let event = match event {
                    CallEvent::Signal { to, data } => {
                        if let Err(err) = to_server.send(&ClientMsg::Signal { to, data }).await {
                            break Err(err);
                        }
                        continue;
                    }
                    CallEvent::PeerState { id, state } => Event::PeerState { id, state },
                    CallEvent::PeerMuted { id, muted } => Event::PeerMuted { id, muted },
                    CallEvent::PeerLevel { id, db } => Event::PeerLevel { id, db },
                    CallEvent::MicLevel { db } => Event::MicLevel { db },
                    CallEvent::PeerStats { id, stats } => Event::PeerStats { id, stats },
                    CallEvent::Chat { from, text } => Event::Chat { from, text },
                    CallEvent::ClickDelay(delay) => Event::ClickDelay(delay),
                };
                let _ = events.send(event);
            }
            Some(message) = bus.next() => {
                if let Err(err) = call.handle_bus_message(&message) {
                    break Err(err);
                }
            }
            _ = stats_tick.tick() => call.request_stats(),
            command = commands.recv() => match command {
                Some(Command::SetMuted(muted)) => call.set_muted(muted),
                Some(Command::SetDeafened(deafened)) => call.set_deafened(deafened),
                Some(Command::SetPeerVolume { id, volume }) => call.set_peer_volume(id, volume),
                Some(Command::SendChat(text)) => call.send_chat(&text),
                // A dropped command channel means the interface is gone.
                Some(Command::Leave) | None => break Ok(()),
            },
        }
    };
    call.shutdown();
    outcome
}
