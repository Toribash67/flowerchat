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

pub use crate::audio::AudioSettings;
use crate::call::{Call, CallConfig, CallEvent};
pub use crate::call::{ConnectionState, PeerStats, Route};
use crate::ice::IceConfig;
use crate::signaling;
pub use crate::video::{
    Codec, FrameSink, ScreenSource, ShareCodec, ShareSettings, ShareSource, Sharing, VideoFrame,
    WatchStats,
};

/// How often connection statistics are refreshed.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct SessionConfig {
    /// WebSocket address of the signaling server.
    pub server: String,
    pub room: String,
    pub name: String,
    pub audio: AudioSettings,
    /// Whether the microphone is sent from the start. Push-to-talk starts
    /// without, and turns it on with [`Command::SetTransmitting`].
    pub transmitting: bool,
    /// Send measurement clicks instead of the microphone.
    pub test_tone: bool,
    /// Time incoming clicks instead of playing audio.
    pub measure: bool,
    /// Where the frames of the screens being watched go. Without it they
    /// are decoded and counted, but not shown.
    pub frames: Option<FrameSink>,
    /// Encode and decode video on the processor even where the graphics
    /// card could: for trying that out, or for a card that lets one down.
    pub software_video: bool,
    /// For testing: the share, from 0 to 1, of arriving packets to lose, as
    /// a poor network would.
    pub incoming_loss: f64,
}

#[derive(Debug, Clone)]
pub enum Command {
    SetMuted(bool),
    SetDeafened(bool),
    /// Lets the microphone out or holds it back, for push-to-talk. The
    /// peers are not told, unlike with muting.
    SetTransmitting(bool),
    /// Changes devices, processing or the receive buffer during the call.
    SetAudio(AudioSettings),
    /// Playback gain for one peer, 1.0 being unchanged.
    SetPeerVolume {
        id: PeerId,
        volume: f64,
    },
    /// Playback gain for the sound of one peer's shared screen.
    SetStreamVolume {
        id: PeerId,
        volume: f64,
    },
    SendChat(String),
    /// Starts sharing a screen with whoever in the room asks to watch.
    StartShare {
        source: ShareSource,
        settings: ShareSettings,
    },
    /// Changes the size, frame rate, bitrate or codec of what we share, or
    /// whether sound goes with it, without a break.
    SetShareSettings(ShareSettings),
    StopShare,
    /// Starts or stops watching what a peer shares.
    Watch {
        id: PeerId,
        watch: bool,
    },
    /// Says how tall, in pixels, a peer's shared screen is shown at most.
    /// Its frames then come no larger than that, which saves the work of
    /// preparing pixels that would not be seen. Zero, the default, asks
    /// for the frames at the size they are sent.
    SetViewHeight {
        id: PeerId,
        max_height: u32,
    },
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
    /// What we are sharing as of now, about once a second while we do, or
    /// that we no longer are.
    OwnShare(Option<Sharing>),
    /// Our share could not start or broke down. It has stopped.
    ShareFailed(String),
    /// What a peer is sharing as of now, about once a second while it does,
    /// or that it no longer is.
    PeerSharing {
        id: PeerId,
        sharing: Option<Sharing>,
    },
    /// How watching a peer's screen has gone over the last second.
    Watching {
        id: PeerId,
        stats: WatchStats,
    },
    /// Loudness of the sound of a peer's shared screen, in dB below full
    /// scale, about twenty times a second while there is any.
    StreamLevel {
        id: PeerId,
        db: f32,
    },
    /// A peer's screen could not be watched.
    WatchFailed {
        id: PeerId,
        reason: String,
    },
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
            audio: config.audio,
            transmitting: config.transmitting,
            test_tone: config.test_tone,
            measure: config.measure,
            ice: IceConfig::from_servers(&welcome.ice_servers),
            frames: config.frames,
            software_video: config.software_video,
            incoming_loss: config.incoming_loss,
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
                    CallEvent::OwnShare(sharing) => Event::OwnShare(sharing),
                    CallEvent::ShareFormat => {
                        call.announce_share();
                        continue;
                    }
                    CallEvent::ShareEnded(reason) => {
                        call.stop_share();
                        match reason {
                            Some(reason) => Event::ShareFailed(reason),
                            None => continue,
                        }
                    }
                    CallEvent::WatchRequest { from, watch } => {
                        call.set_watcher(from, watch);
                        continue;
                    }
                    CallEvent::PeerSharing { id, sharing } => {
                        if sharing.is_none() {
                            call.peer_stopped_sharing(id);
                        }
                        Event::PeerSharing { id, sharing }
                    }
                    CallEvent::Watching { id, stats } => Event::Watching { id, stats },
                    CallEvent::StreamLevel { id, db } => Event::StreamLevel { id, db },
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
                Some(Command::SetTransmitting(on)) => call.set_transmitting(on),
                Some(Command::SetAudio(audio)) => {
                    if let Err(err) = call.apply_audio(audio) {
                        break Err(err);
                    }
                }
                Some(Command::SetPeerVolume { id, volume }) => call.set_peer_volume(id, volume),
                Some(Command::SetStreamVolume { id, volume }) => {
                    call.set_stream_volume(id, volume);
                }
                Some(Command::SendChat(text)) => call.send_chat(&text),
                Some(Command::StartShare { source, settings }) => {
                    if let Err(err) = call.start_share(&source, settings) {
                        let _ = events.send(Event::ShareFailed(format!("{err:#}")));
                    }
                }
                Some(Command::SetShareSettings(settings)) => call.set_share_settings(settings),
                Some(Command::StopShare) => call.stop_share(),
                Some(Command::SetViewHeight { id, max_height }) => {
                    call.set_view_height(id, max_height);
                }
                Some(Command::Watch { id, watch }) => {
                    if let Err(err) = call.watch(id, watch) {
                        let reason = format!("{err:#}");
                        let _ = events.send(Event::WatchFailed { id, reason });
                    }
                }
                // A dropped command channel means the interface is gone.
                Some(Command::Leave) | None => break Ok(()),
            },
        }
    };
    call.shutdown();
    outcome
}
