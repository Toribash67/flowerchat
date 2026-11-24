//! The media side of a call: one GStreamer pipeline that captures and
//! encodes the microphone once, sends it to every peer, and mixes what the
//! peers send back.
//!
//! ```text
//! {mic -> processing} -> opusenc -> tee -+-> [queue -> rtpopuspay] -> webrtcbin (peer A)
//!                                        +-> [queue -> rtpopuspay] -> webrtcbin (peer B)
//!
//! webrtcbin (peer A) -> [rtpopusdepay -> opusdec] -+-> audiomixer -> speakers
//! webrtcbin (peer B) -> [rtpopusdepay -> opusdec] -+
//! ```
//!
//! The bracketed parts and the `webrtcbin`s come and go with the peers. The
//! part in braces is replaced when the microphone or its processing changes,
//! and the speakers are reopened when they change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flowerchat_protocol::PeerId;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, trace, warn};

use crate::audio::{
    AudioSettings, LEVEL_INTERVAL_NS, MAX_JITTER_MS, RAW_AUDIO, capture_bin, level_db,
    unique_probe_name,
};
use crate::ice::IceConfig;
use crate::latency::{ClickDetector, write_clicks};

/// Payload type we propose for Opus when we make the offer. An answer has to
/// reuse whatever the offer picked instead.
const OPUS_PAYLOAD_TYPE: i32 = 111;
/// Extra time the mixer waits for decoded audio before giving up on a peer
/// for the current 10 ms slice.
const MIXER_SLACK_NS: u64 = 10_000_000;
/// Time the sink allows for decoding and mixing a buffer before it is due.
/// Sinks default to 20 ms, which is far more than this pipeline needs, and
/// every millisecond of it is added to the playback delay.
const SINK_DEADLINE_NS: u64 = 5_000_000;
/// Longest to wait for playback to reach a point where the speakers can be
/// swapped. The mixer produces audio every 10 ms, so this only runs out when
/// playback has already stopped.
const SPEAKER_SWAP_WAIT: Duration = Duration::from_millis(500);
const MIC_LEVEL: &str = "mic_level";
/// Prefix of the level element in a peer's receive branch; the id follows.
const PEER_LEVEL_PREFIX: &str = "level-peer-";

pub struct CallConfig {
    pub audio: AudioSettings,
    /// Whether the microphone is sent from the start. Push-to-talk starts
    /// without.
    pub transmitting: bool,
    /// Send measurement clicks instead of the microphone.
    pub test_tone: bool,
    /// Time incoming clicks instead of playing audio.
    pub measure: bool,
    pub ice: IceConfig,
}

/// Things the call reports back from GStreamer's threads.
#[derive(Debug)]
pub enum CallEvent {
    /// Signaling to pass on to a peer.
    Signal {
        to: PeerId,
        data: Value,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// How the media reaches a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Straight between the two machines.
    Direct,
    /// Through a TURN relay.
    Relayed,
}

/// What a connection reports about itself. A field is `None` until the
/// connection has produced the data for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerStats {
    pub route: Option<Route>,
    /// Network round trip to the peer and back.
    pub round_trip_ms: Option<f64>,
    /// Share of the peer's packets that never arrived, since the last report.
    pub loss_percent: Option<f64>,
    /// Variation in packet arrival times.
    pub jitter_ms: Option<f64>,
    pub receiving_kbps: Option<f64>,
    pub sending_kbps: Option<f64>,
}

pub struct Call {
    pipeline: gst::Pipeline,
    /// Whatever feeds `mic_gate`: the microphone chain or the click source.
    capture: Mutex<gst::Bin>,
    mic_tee: gst::Element,
    mic_gate: gst::Element,
    mixer: gst::Element,
    out_gate: gst::Element,
    /// The sound device we play on, and the pad that feeds it. Absent when
    /// measuring.
    speakers: Option<(gst::Element, gst::Pad)>,
    /// Name of the element that hears what the speakers play, if any.
    echo_probe: Option<String>,
    /// The settings in effect.
    audio: Mutex<AudioSettings>,
    test_tone: bool,
    ice: IceConfig,
    events: UnboundedSender<CallEvent>,
    peers: Mutex<HashMap<PeerId, Arc<Peer>>>,
    /// Shared with the control channels, which announce it when they open.
    muted: Arc<AtomicBool>,
    /// Whether push-to-talk, if in use, currently lets the microphone out.
    transmitting: AtomicBool,
}

struct Peer {
    id: PeerId,
    name: String,
    webrtc: gst::Element,
    /// Set from the moment we decide to offer until the answer arrives.
    /// Offers and candidates received in that window belong to an offer the
    /// peer made at the same time, which loses to ours.
    awaiting_answer: AtomicBool,
    send: Mutex<Option<SendBranch>>,
    recv: Mutex<Option<RecvBranch>>,
    control: Mutex<Option<gst_webrtc::WebRTCDataChannel>>,
    /// Playback gain for this peer, 1.0 being unchanged.
    volume: Mutex<f64>,
    /// Counters from the previous stats report, for rates.
    counters: Mutex<Option<Counters>>,
}

/// Cumulative counters at the time of a stats report.
#[derive(Clone, Copy)]
struct Counters {
    packets_received: f64,
    packets_lost: f64,
}

struct SendBranch {
    bin: gst::Bin,
    tee_pad: gst::Pad,
}

struct RecvBranch {
    bin: gst::Bin,
    mixer_pad: gst::Pad,
    volume: gst::Element,
}

impl Call {
    pub fn new(config: CallConfig, events: UnboundedSender<CallEvent>) -> Result<Self> {
        let CallConfig {
            mut audio,
            transmitting,
            test_tone,
            measure,
            ice,
        } = config;
        audio.jitter_ms = audio.jitter_ms.min(MAX_JITTER_MS);

        // Echo cancellation needs the playback signal, which only exists
        // when we actually play audio.
        let echo_probe = (!measure && !test_tone).then(unique_probe_name);
        let playback = if measure {
            format!("appsink name=detector sync=true processing-deadline={SINK_DEADLINE_NS}")
        } else {
            let probe = echo_probe
                .as_ref()
                .map_or_else(String::new, |name| format!("webrtcechoprobe name={name} !"));
            format!(
                "{probe} audioconvert ! audioresample name=to_speakers \
                 ! pulsesink name=speakers buffer-time=20000 latency-time=10000 \
                   processing-deadline={SINK_DEADLINE_NS}"
            )
        };
        let jitter_ns = u64::from(audio.jitter_ms) * 1_000_000;
        let description = format!(
            "volume name=mic_gate \
             ! level name={MIC_LEVEL} interval={LEVEL_INTERVAL_NS} post-messages=true \
             ! opusenc audio-type=voice bitrate=64000 frame-size=10 inband-fec=true \
               packet-loss-percentage=5 \
             ! tee name=mic_tee allow-not-linked=true \
             audiomixer name=mixer force-live=true latency={MIXER_SLACK_NS} \
               min-upstream-latency={jitter_ns} \
             ! {RAW_AUDIO} ! volume name=out_gate ! {playback}"
        );
        debug!(%description, "building pipeline");

        let pipeline = gst::parse::launch(&description)
            .context("cannot build the audio pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("the audio pipeline is not a pipeline"))?;
        let by_name = |name: &str| {
            pipeline
                .by_name(name)
                .ok_or_else(|| anyhow!("pipeline element {name} is missing"))
        };
        let mic_gate = by_name("mic_gate")?;

        let capture = if test_tone {
            let clicks = gst::parse::bin_from_description(
                &format!(
                    "audiotestsrc is-live=true wave=silence samplesperbuffer=480 \
                     ! capsfilter name=clicks caps={RAW_AUDIO}"
                ),
                true,
            )?;
            let source = clicks
                .by_name("clicks")
                .ok_or_else(|| anyhow!("the click source is missing"))?;
            install_click_writer(&source)?;
            clicks
        } else {
            capture_bin(&audio, echo_probe.as_deref())?
        };
        pipeline.add(&capture)?;
        capture.link(&mic_gate)?;

        let speakers = if measure {
            install_click_detector(&by_name("detector")?, events.clone())?;
            None
        } else {
            let speakers = by_name("speakers")?;
            if let Some(device) = &audio.output {
                speakers.set_property("device", device);
            }
            let feed = by_name("to_speakers")?
                .static_pad("src")
                .ok_or_else(|| anyhow!("nothing feeds the speakers"))?;
            Some((speakers, feed))
        };

        let call = Self {
            capture: Mutex::new(capture),
            mic_tee: by_name("mic_tee")?,
            mic_gate,
            mixer: by_name("mixer")?,
            out_gate: by_name("out_gate")?,
            speakers,
            echo_probe,
            audio: Mutex::new(audio),
            test_tone,
            ice,
            pipeline,
            events,
            peers: Mutex::new(HashMap::new()),
            muted: Arc::new(AtomicBool::new(false)),
            transmitting: AtomicBool::new(transmitting),
        };
        call.update_mic_gate();
        call.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the audio pipeline (is the audio device available?)")?;
        Ok(call)
    }

    pub fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("pipelines always have a bus")
    }

    /// Reacts to a message from the pipeline's bus. Fails when the pipeline
    /// itself has failed, after which the call cannot continue.
    pub fn handle_bus_message(&self, message: &gst::Message) -> Result<()> {
        match message.view() {
            gst::MessageView::Error(err) => {
                let source = err.src().map(|s| s.path_string()).unwrap_or_default();
                bail!(
                    "audio pipeline failed in {source}: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
            // Newly added live elements change the pipeline's latency.
            gst::MessageView::Latency(_) => {
                let _ = self.pipeline.recalculate_latency();
            }
            gst::MessageView::Element(_) => {
                let source = message.src().map(|s| s.name()).unwrap_or_default();
                if let Some(db) = level_db(message) {
                    if source == MIC_LEVEL {
                        let _ = self.events.send(CallEvent::MicLevel { db });
                    } else if let Some(id) = source
                        .strip_prefix(PEER_LEVEL_PREFIX)
                        .and_then(|id| id.parse().ok())
                    {
                        let _ = self.events.send(CallEvent::PeerLevel { id, db });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Stops or resumes sending the microphone, and tells the peers.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        self.update_mic_gate();
        self.broadcast(&json!({ "muted": muted }));
    }

    /// Lets the microphone out or holds it back, for push-to-talk. Unlike
    /// muting, this is not announced: to the peers, someone who is not
    /// holding the key is simply quiet.
    pub fn set_transmitting(&self, transmitting: bool) {
        self.transmitting.store(transmitting, Ordering::SeqCst);
        self.update_mic_gate();
    }

    fn update_mic_gate(&self) {
        let open = self.transmitting.load(Ordering::SeqCst) && !self.muted.load(Ordering::SeqCst);
        self.mic_gate.set_property("mute", !open);
    }

    /// Changes the audio settings of the running call. Fails when the
    /// pipeline cannot be rearranged, after which the call cannot continue.
    /// A device that does not open is reported later, through the bus.
    pub fn apply_audio(&self, mut new: AudioSettings) -> Result<()> {
        new.jitter_ms = new.jitter_ms.min(MAX_JITTER_MS);
        let old = std::mem::replace(&mut *self.audio.lock().unwrap(), new.clone());
        if !self.test_tone && !old.captures_like(&new) {
            self.replace_capture(&new)?;
        }
        if old.output != new.output {
            self.reopen_speakers(new.output.as_deref())?;
        }
        if old.jitter_ms != new.jitter_ms {
            for peer in self.peers.lock().unwrap().values() {
                peer.webrtc.set_property("latency", new.jitter_ms);
            }
            self.mixer
                .set_property("min-upstream-latency", u64::from(new.jitter_ms) * 1_000_000);
            let _ = self.pipeline.recalculate_latency();
        }
        Ok(())
    }

    /// Swaps the microphone chain for one built from `settings`.
    fn replace_capture(&self, settings: &AudioSettings) -> Result<()> {
        let new = capture_bin(settings, self.echo_probe.as_deref())?;
        let mut capture = self.capture.lock().unwrap();
        // The chain contains its own source, so stopping it also stops the
        // flow into the rest of the pipeline, which just sees a pause.
        let _ = capture.set_state(gst::State::Null);
        capture.unlink(&self.mic_gate);
        self.pipeline.remove(&*capture)?;

        self.pipeline.add(&new)?;
        new.link(&self.mic_gate)?;
        new.sync_state_with_parent()
            .context("cannot start the microphone")?;
        *capture = new;
        Ok(())
    }

    /// Closes the sound device we play on and opens `device` instead.
    ///
    /// The element stays the same one, because its clock is the clock the
    /// whole pipeline runs on. That clock stands still while the device is
    /// closed and carries on from where it was.
    fn reopen_speakers(&self, device: Option<&str>) -> Result<()> {
        let Some((speakers, feed)) = &self.speakers else {
            return Ok(());
        };
        let sink_pad = speakers
            .static_pad("sink")
            .ok_or_else(|| anyhow!("the speakers have no sink pad"))?;

        // Hold playback back at a moment when nothing is on its way into
        // the device, and keep it there until the new device is in place.
        let (blocked_tx, blocked) = mpsc::sync_channel(1);
        let block = feed
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, move |_, _| {
                let _ = blocked_tx.try_send(());
                gst::PadProbeReturn::Ok
            })
            .ok_or_else(|| anyhow!("cannot hold playback back"))?;
        let _ = blocked.recv_timeout(SPEAKER_SWAP_WAIT);

        let swap = || -> Result<()> {
            feed.unlink(&sink_pad)?;
            speakers.set_state(gst::State::Ready)?;
            speakers.set_property("device", device);
            // Linking again makes the feed repeat the format and timing
            // information that the device forgot when it closed.
            feed.link(&sink_pad)?;
            speakers.sync_state_with_parent()?;
            Ok(())
        };
        let outcome = swap().context("cannot switch speakers");
        feed.remove_probe(block);
        outcome
    }

    /// Silences or restores everything we play.
    pub fn set_deafened(&self, deafened: bool) {
        self.out_gate.set_property("mute", deafened);
    }

    /// Sets how loud one peer is played, 1.0 being unchanged.
    pub fn set_peer_volume(&self, id: PeerId, volume: f64) {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return;
        };
        *peer.volume.lock().unwrap() = volume;
        if let Some(recv) = &*peer.recv.lock().unwrap() {
            recv.volume.set_property("volume", volume);
        }
    }

    /// Sends a chat message to everyone we are connected to.
    pub fn send_chat(&self, text: &str) {
        self.broadcast(&json!({ "chat": text }));
    }

    /// Asks every connection for its statistics; they arrive as events.
    pub fn request_stats(&self) {
        for peer in self.peers.lock().unwrap().values() {
            let events = self.events.clone();
            let weak_peer = Arc::downgrade(peer);
            let promise = gst::Promise::with_change_func(move |reply| {
                let (Ok(Some(reply)), Some(peer)) = (reply, weak_peer.upgrade()) else {
                    return;
                };
                trace!(peer = peer.id, "raw stats: {reply:?}");
                let stats = read_stats(reply, &mut peer.counters.lock().unwrap());
                let _ = events.send(CallEvent::PeerStats { id: peer.id, stats });
            });
            peer.webrtc
                .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        }
    }

    fn broadcast(&self, message: &Value) {
        let text = message.to_string();
        for peer in self.peers.lock().unwrap().values() {
            if let Some(channel) = &*peer.control.lock().unwrap()
                && channel.ready_state() == gst_webrtc::WebRTCDataChannelState::Open
            {
                let _ = channel.send_string_full(Some(&text));
            }
        }
    }

    /// Starts a connection to `id`. Exactly one side must pass `we_offer`.
    pub fn add_peer(&self, id: PeerId, name: &str, we_offer: bool) -> Result<()> {
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(format!("peer-{id}"))
            .property_from_str("bundle-policy", "max-bundle")
            .property("latency", self.audio.lock().unwrap().jitter_ms)
            .build()
            .context("cannot create webrtcbin")?;
        if let Some(stun) = &self.ice.stun {
            webrtc.set_property("stun-server", stun);
        }
        for turn in &self.ice.turn {
            if !webrtc.emit_by_name::<bool>("add-turn-server", &[turn]) {
                warn!("webrtcbin rejected a TURN server URI");
            }
        }

        let peer = Arc::new(Peer {
            id,
            name: name.to_owned(),
            webrtc: webrtc.clone(),
            awaiting_answer: AtomicBool::new(we_offer),
            send: Mutex::new(None),
            recv: Mutex::new(None),
            control: Mutex::new(None),
            volume: Mutex::new(1.0),
            counters: Mutex::new(None),
        });

        let events = self.events.clone();
        webrtc.connect("on-ice-candidate", false, move |values| {
            let mline = values[1].get::<u32>().expect("mline index is a u32");
            let candidate = values[2].get::<String>().expect("candidate is a string");
            let data = json!({ "candidate": { "candidate": candidate, "sdpMLineIndex": mline } });
            let _ = events.send(CallEvent::Signal { to: id, data });
            None
        });

        let events = self.events.clone();
        webrtc.connect_notify(Some("connection-state"), move |webrtc, _| {
            use gst_webrtc::WebRTCPeerConnectionState as S;
            let state = match webrtc.property::<S>("connection-state") {
                S::Connected => ConnectionState::Connected,
                S::Disconnected => ConnectionState::Disconnected,
                S::Failed => ConnectionState::Failed,
                S::Closed => ConnectionState::Closed,
                _ => ConnectionState::Connecting,
            };
            let _ = events.send(CallEvent::PeerState { id, state });
        });

        let pipeline = self.pipeline.downgrade();
        let mixer = self.mixer.clone();
        let weak_peer = Arc::downgrade(&peer);
        webrtc.connect_pad_added(move |_, pad| {
            let (Some(pipeline), Some(peer)) = (pipeline.upgrade(), weak_peer.upgrade()) else {
                return;
            };
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            if let Err(err) = attach_recv_branch(&pipeline, &mixer, &peer, pad) {
                warn!(peer = peer.id, "cannot play incoming audio: {err:#}");
            }
        });

        self.pipeline.add(&webrtc)?;
        if we_offer {
            // As the offerer we pick the payload type. As the answerer we
            // have to wait for the offer to learn it.
            self.attach_send_branch(&peer, OPUS_PAYLOAD_TYPE)?;
        }

        // Data channels can only be created once the element has left NULL.
        webrtc.set_state(gst::State::Ready)?;
        self.open_control_channel(&peer)?;
        webrtc.sync_state_with_parent()?;

        self.peers.lock().unwrap().insert(id, peer.clone());
        if we_offer {
            self.send_offer(&peer);
        }
        Ok(())
    }

    /// Handles a `signal` payload from `from`: a session description or an
    /// ICE candidate, in the shape browsers produce.
    pub fn on_signal(&self, from: PeerId, data: &Value) -> Result<()> {
        let Some(peer) = self.peers.lock().unwrap().get(&from).cloned() else {
            return Ok(());
        };

        if let Some(description) = data.get("description") {
            let kind = description["type"].as_str().unwrap_or_default();
            let sdp_text = description["sdp"]
                .as_str()
                .ok_or_else(|| anyhow!("description without sdp"))?;
            let sdp = gst_sdp::SDPMessage::parse_buffer(sdp_text.as_bytes())
                .context("peer sent SDP that does not parse")?;
            debug!(peer = from, kind, "received description:\n{sdp_text}");
            match kind {
                "offer" => self.on_offer(&peer, sdp)?,
                "answer" => {
                    let answer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        sdp,
                    );
                    peer.webrtc.emit_by_name::<()>(
                        "set-remote-description",
                        &[&answer, &None::<gst::Promise>],
                    );
                    peer.awaiting_answer.store(false, Ordering::SeqCst);
                }
                other => bail!("unsupported description type {other:?}"),
            }
        } else if let Some(candidate) = data.get("candidate") {
            if peer.awaiting_answer.load(Ordering::SeqCst) {
                return Ok(());
            }
            let text = candidate["candidate"].as_str().unwrap_or_default();
            // An empty candidate only marks the end of gathering.
            if text.is_empty() {
                return Ok(());
            }
            let mline = candidate["sdpMLineIndex"].as_u64().unwrap_or(0) as u32;
            peer.webrtc
                .emit_by_name::<()>("add-ice-candidate", &[&mline, &text]);
        }
        Ok(())
    }

    pub fn remove_peer(&self, id: PeerId) {
        let Some(peer) = self.peers.lock().unwrap().remove(&id) else {
            return;
        };
        let pipeline = self.pipeline.clone();
        let mic_tee = self.mic_tee.clone();
        let mixer = self.mixer.clone();
        let send = peer.send.lock().unwrap().take();

        let Some(send) = send else {
            pipeline.call_async(move |pipeline| teardown(pipeline, &mic_tee, &mixer, &peer, None));
            return;
        };
        // The branch must be cut off from the tee while no buffer is passing
        // through it. Shutting it down while still linked would return an
        // error to the tee and stop the microphone for everyone.
        let tee_pad = send.tee_pad.clone();
        let pending = Mutex::new(Some((pipeline, mic_tee, mixer, peer, send)));
        tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
            if let Some((pipeline, mic_tee, mixer, peer, send)) = pending.lock().unwrap().take() {
                if let Some(sink) = pad.peer() {
                    let _ = pad.unlink(&sink);
                }
                // Stopping elements can block, so it happens off this thread.
                pipeline.call_async(move |pipeline| {
                    teardown(pipeline, &mic_tee, &mixer, &peer, Some(send));
                });
            }
            gst::PadProbeReturn::Remove
        });
    }

    pub fn shutdown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    fn on_offer(&self, peer: &Arc<Peer>, sdp: gst_sdp::SDPMessage) -> Result<()> {
        if peer.awaiting_answer.load(Ordering::SeqCst) {
            debug!(peer = peer.id, "ignoring an offer that collided with ours");
            return Ok(());
        }
        if peer.send.lock().unwrap().is_none() {
            let payload_type = opus_payload_type(&sdp)
                .ok_or_else(|| anyhow!("the offer does not include Opus audio"))?;
            self.attach_send_branch(peer, payload_type)?;
        }

        let offer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
        peer.webrtc
            .emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

        let events = self.events.clone();
        let webrtc = peer.webrtc.clone();
        let id = peer.id;
        let promise = gst::Promise::with_change_func(move |reply| {
            match description_from_reply(reply, "answer") {
                Ok(answer) => apply_and_send(&webrtc, &events, id, "answer", &answer),
                Err(err) => warn!(peer = id, "cannot create an answer: {err:#}"),
            }
        });
        peer.webrtc
            .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
        Ok(())
    }

    fn send_offer(&self, peer: &Arc<Peer>) {
        let events = self.events.clone();
        let webrtc = peer.webrtc.clone();
        let id = peer.id;
        let promise = gst::Promise::with_change_func(move |reply| {
            match description_from_reply(reply, "offer") {
                Ok(offer) => apply_and_send(&webrtc, &events, id, "offer", &offer),
                Err(err) => warn!(peer = id, "cannot create an offer: {err:#}"),
            }
        });
        peer.webrtc
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    }

    /// Connects the shared encoded microphone stream to this peer.
    fn attach_send_branch(&self, peer: &Peer, payload_type: i32) -> Result<()> {
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48_000i32)
            .field("encoding-params", "2")
            .field("payload", payload_type)
            .build();
        // The queue is leaky so that a peer that stops taking data cannot
        // hold up the microphone for the others.
        let bin = gst::parse::bin_from_description(
            &format!(
                "queue leaky=downstream max-size-buffers=20 max-size-time=0 max-size-bytes=0 \
                 ! rtpopuspay pt={payload_type} ! capsfilter caps=\"{caps}\""
            ),
            true,
        )?;
        self.pipeline.add(&bin)?;

        let template = peer
            .webrtc
            .pad_template("sink_%u")
            .ok_or_else(|| anyhow!("webrtcbin has no sink pad template"))?;
        let webrtc_sink = peer
            .webrtc
            .request_pad(&template, None, Some(&caps))
            .ok_or_else(|| anyhow!("webrtcbin refused a sink pad"))?;
        bin.static_pad("src")
            .ok_or_else(|| anyhow!("send branch has no src pad"))?
            .link(&webrtc_sink)?;

        bin.sync_state_with_parent()?;
        let tee_pad = self
            .mic_tee
            .request_pad_simple("src_%u")
            .ok_or_else(|| anyhow!("tee refused a src pad"))?;
        tee_pad.link(
            &bin.static_pad("sink")
                .ok_or_else(|| anyhow!("send branch has no sink pad"))?,
        )?;

        *peer.send.lock().unwrap() = Some(SendBranch { bin, tee_pad });
        Ok(())
    }

    /// Opens the per-peer data channel. Both sides create it with the same
    /// id, so it needs no negotiation of its own.
    fn open_control_channel(&self, peer: &Peer) -> Result<()> {
        let options = gst::Structure::builder("options")
            .field("negotiated", true)
            .field("id", 0i32)
            .build();
        let channel = peer
            .webrtc
            .emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
                "create-data-channel",
                &[&"control", &options],
            )
            .ok_or_else(|| anyhow!("webrtcbin could not create the control channel"))?;

        let muted = self.muted.clone();
        channel.connect_on_open(move |channel| {
            let state = json!({ "muted": muted.load(Ordering::SeqCst) });
            let _ = channel.send_string_full(Some(&state.to_string()));
        });
        let events = self.events.clone();
        let id = peer.id;
        channel.connect_on_message_string(move |_, message| {
            let Some(msg) = message.and_then(|text| serde_json::from_str::<Value>(text).ok())
            else {
                return;
            };
            if let Some(muted) = msg["muted"].as_bool() {
                let _ = events.send(CallEvent::PeerMuted { id, muted });
            }
            if let Some(text) = msg["chat"].as_str() {
                let text = text.to_owned();
                let _ = events.send(CallEvent::Chat { from: id, text });
            }
        });
        *peer.control.lock().unwrap() = Some(channel);
        Ok(())
    }
}

impl Drop for Call {
    /// A call that is dropped without being shut down, because its session
    /// was cancelled, must still stop its pipeline and let go of the audio
    /// devices.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Decodes a peer's audio and feeds it to the mixer.
fn attach_recv_branch(
    pipeline: &gst::Pipeline,
    mixer: &gst::Element,
    peer: &Peer,
    webrtc_src: &gst::Pad,
) -> Result<()> {
    // Lost packets are concealed (plc) but not recovered from Opus in-band
    // FEC: with use-inband-fec, opusdec reports 120 ms of extra latency.
    // The level is taken before the volume control, so the speaking indicator
    // shows what the peer sends, not how loud we chose to play it.
    let bin = gst::parse::bin_from_description(
        &format!(
            "rtpopusdepay ! opusdec plc=true \
             ! audioconvert ! audioresample ! capsfilter caps={RAW_AUDIO} \
             ! level name={PEER_LEVEL_PREFIX}{} interval={LEVEL_INTERVAL_NS} post-messages=true \
             ! volume name=volume",
            peer.id
        ),
        true,
    )?;
    let volume = bin
        .by_name("volume")
        .ok_or_else(|| anyhow!("receive branch has no volume control"))?;
    volume.set_property("volume", *peer.volume.lock().unwrap());
    pipeline.add(&bin)?;
    let mixer_pad = mixer
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("mixer refused a sink pad"))?;
    bin.static_pad("src")
        .ok_or_else(|| anyhow!("receive branch has no src pad"))?
        .link(&mixer_pad)?;
    bin.sync_state_with_parent()?;
    webrtc_src.link(
        &bin.static_pad("sink")
            .ok_or_else(|| anyhow!("receive branch has no sink pad"))?,
    )?;
    debug!(peer = peer.id, name = %peer.name, "receiving audio");
    *peer.recv.lock().unwrap() = Some(RecvBranch {
        bin,
        mixer_pad,
        volume,
    });
    Ok(())
}

/// Turns webrtcbin's statistics into the figures we show. `previous` holds
/// the counters from the last report and is updated for the next one.
///
/// The report is a structure with one nested structure per statistics
/// object, named after its kind (`inbound-rtp`, `candidate-pair`, ...).
fn read_stats(stats: &gst::StructureRef, previous: &mut Option<Counters>) -> PeerStats {
    let mut out = PeerStats::default();
    let entry = |id: &str| stats.get::<gst::Structure>(id).ok();
    let text = |s: &gst::StructureRef, field: &str| s.get::<String>(field).ok();

    for (_, value) in stats.iter() {
        let Ok(object) = value.get::<gst::Structure>() else {
            continue;
        };
        let is_audio = text(&object, "kind").as_deref() == Some("audio");
        match object.name().as_str() {
            "inbound-rtp" if is_audio => {
                out.jitter_ms = number(&object, "jitter").map(|seconds| seconds * 1000.0);
                out.receiving_kbps = source_kbps(&object);
                let now = Counters {
                    packets_received: number(&object, "packets-received").unwrap_or(0.0),
                    packets_lost: number(&object, "packets-lost").unwrap_or(0.0),
                };
                if let Some(before) = previous.replace(now) {
                    let lost = (now.packets_lost - before.packets_lost).max(0.0);
                    let expected = now.packets_received - before.packets_received + lost;
                    if expected > 0.0 {
                        out.loss_percent = Some(100.0 * lost / expected);
                    }
                }
            }
            "outbound-rtp" if is_audio => out.sending_kbps = source_kbps(&object),
            "remote-inbound-rtp" if is_audio => {
                out.round_trip_ms =
                    number(&object, "round-trip-time").map(|seconds| seconds * 1000.0);
            }
            "transport" => {
                let candidate_type = |pair: &gst::Structure, end: &str| {
                    let candidate = entry(&text(pair, end)?)?;
                    text(&candidate, "candidate-type")
                };
                let pair = text(&object, "selected-candidate-pair-id").and_then(|id| entry(&id));
                if let Some(pair) = pair {
                    let local = candidate_type(&pair, "local-candidate-id");
                    let remote = candidate_type(&pair, "remote-candidate-id");
                    if local.is_some() || remote.is_some() {
                        let relayed = [local, remote].iter().flatten().any(|t| t == "relay");
                        out.route = Some(if relayed {
                            Route::Relayed
                        } else {
                            Route::Direct
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A numeric field, whichever integer or float type GStreamer gave it.
fn number(s: &gst::StructureRef, field: &str) -> Option<f64> {
    let value = s.value(field).ok()?;
    None.or_else(|| value.get::<f64>().ok())
        .or_else(|| value.get::<u64>().ok().map(|v| v as f64))
        .or_else(|| value.get::<i64>().ok().map(|v| v as f64))
        .or_else(|| value.get::<u32>().ok().map(f64::from))
        .or_else(|| value.get::<i32>().ok().map(f64::from))
}

/// The bitrate of an RTP stream including packet headers, as measured by the
/// RTP session itself.
fn source_kbps(rtp: &gst::StructureRef) -> Option<f64> {
    let source = rtp.get::<gst::Structure>("gst-rtpsource-stats").ok()?;
    number(&source, "bitrate").map(|bits_per_second| bits_per_second / 1000.0)
}

/// Stops and removes everything that belonged to one peer. The send branch
/// must already be unlinked from the tee.
fn teardown(
    pipeline: &gst::Pipeline,
    mic_tee: &gst::Element,
    mixer: &gst::Element,
    peer: &Peer,
    send: Option<SendBranch>,
) {
    if let Some(channel) = peer.control.lock().unwrap().take() {
        channel.close();
    }
    // Stopping webrtcbin first ends the flow into the receive branch.
    let _ = peer.webrtc.set_state(gst::State::Null);
    if let Some(send) = send {
        let _ = send.bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&send.bin);
        mic_tee.release_request_pad(&send.tee_pad);
    }
    if let Some(recv) = peer.recv.lock().unwrap().take() {
        let _ = recv.bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&recv.bin);
        mixer.release_request_pad(&recv.mixer_pad);
    }
    let _ = pipeline.remove(&peer.webrtc);
    debug!(peer = peer.id, "removed");
}

fn description_from_reply(
    reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    field: &str,
) -> Result<gst_webrtc::WebRTCSessionDescription> {
    let reply = reply
        .map_err(|err| anyhow!("promise failed: {err:?}"))?
        .ok_or_else(|| anyhow!("empty reply"))?;
    if let Ok(error) = reply.get::<gst::glib::Error>("error") {
        bail!("{error}");
    }
    reply
        .get::<gst_webrtc::WebRTCSessionDescription>(field)
        .with_context(|| format!("reply has no {field}"))
}

fn apply_and_send(
    webrtc: &gst::Element,
    events: &UnboundedSender<CallEvent>,
    to: PeerId,
    kind: &str,
    description: &gst_webrtc::WebRTCSessionDescription,
) {
    webrtc.emit_by_name::<()>(
        "set-local-description",
        &[description, &None::<gst::Promise>],
    );
    let sdp = match description.sdp().as_text() {
        Ok(sdp) => sdp,
        Err(err) => return warn!(peer = to, "cannot serialise our {kind}: {err}"),
    };
    debug!(peer = to, kind, "sending description:\n{sdp}");
    let data = json!({ "description": { "type": kind, "sdp": sdp } });
    let _ = events.send(CallEvent::Signal { to, data });
}

/// The payload type an offer assigns to Opus.
fn opus_payload_type(sdp: &gst_sdp::SDPMessageRef) -> Option<i32> {
    for media in sdp.medias() {
        if media.media() != Some("audio") {
            continue;
        }
        for format in media.formats() {
            let Ok(payload_type) = format.parse::<i32>() else {
                continue;
            };
            let is_opus = media.caps_from_media(payload_type).is_some_and(|caps| {
                caps.structure(0)
                    .and_then(|s| s.get::<&str>("encoding-name").ok())
                    .is_some_and(|name| name.eq_ignore_ascii_case("opus"))
            });
            if is_opus {
                return Some(payload_type);
            }
        }
    }
    None
}

/// Nanoseconds to add to a time on `clock` to get monotonic system time.
/// Audio devices can supply the pipeline clock, and those do not count from
/// the same origin as the system clock.
fn monotonic_offset(clock: &gst::Clock) -> i128 {
    let system = gst::SystemClock::obtain();
    system.time().nseconds() as i128 - clock.time().nseconds() as i128
}

/// Overwrites the (silent) test source with measurement clicks.
fn install_click_writer(capsfilter: &gst::Element) -> Result<()> {
    let pad = capsfilter
        .static_pad("src")
        .ok_or_else(|| anyhow!("test source has no src pad"))?;
    let element = capsfilter.downgrade();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(element) = element.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        let (Some(clock), Some(base_time)) = (element.clock(), element.base_time()) else {
            return gst::PadProbeReturn::Ok;
        };
        if let Some(buffer) = info.buffer_mut() {
            let Some(pts) = buffer.pts() else {
                return gst::PadProbeReturn::Ok;
            };
            let captured_at = (base_time + pts).nseconds() as i128 + monotonic_offset(&clock);
            let buffer = buffer.make_mut();
            // The source marks its silence as a gap, which lets elements
            // downstream skip the buffer. With clicks in it, it is no gap,
            // and the mute in particular has to process it.
            buffer.unset_flags(gst::BufferFlags::GAP);
            if let Ok(mut map) = buffer.map_writable() {
                let mut samples: Vec<i16> = vec![0; map.len() / 2];
                write_clicks(&mut samples, captured_at.max(0) as u64);
                for (bytes, sample) in map.as_chunks_mut::<2>().0.iter_mut().zip(&samples) {
                    *bytes = sample.to_le_bytes();
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// Times the clicks in the mixed audio at the moment it would be played.
fn install_click_detector(
    appsink: &gst::Element,
    events: UnboundedSender<CallEvent>,
) -> Result<()> {
    let appsink = appsink
        .clone()
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("detector is not an appsink"))?;
    let detector = Mutex::new(ClickDetector::new());
    let system = gst::SystemClock::obtain();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // The sink is synchronised, so the sample is handed over at
                // the moment a sound card would start playing it.
                let played_at = system.time().nseconds();
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let samples: Vec<i16> = map
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| i16::from_le_bytes(*bytes))
                    .collect();
                if let Some(delay) = detector.lock().unwrap().process(&samples, played_at) {
                    let _ = events.send(CallEvent::ClickDelay(delay));
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}
