//! The media side of a call: one GStreamer pipeline that captures and
//! encodes the microphone once, sends it to every peer, and mixes what the
//! peers send back.
//!
//! ```text
//! mic -> processing -> opusenc -> tee -+-> [queue -> rtpopuspay] -> webrtcbin (peer A)
//!                                      +-> [queue -> rtpopuspay] -> webrtcbin (peer B)
//!
//! webrtcbin (peer A) -> [rtpopusdepay -> opusdec] -+-> audiomixer -> speakers
//! webrtcbin (peer B) -> [rtpopusdepay -> opusdec] -+
//! ```
//!
//! The bracketed parts and the `webrtcbin`s come and go with the peers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use flowerchat_protocol::PeerId;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::ice::IceConfig;
use crate::latency::{ClickDetector, write_clicks};

/// The format audio has between capture and encoder, and between decoders
/// and playback. `webrtcdsp` needs exactly this layout.
const RAW_AUDIO: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1";
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

pub struct CallConfig {
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
        state: String,
    },
    PeerMuted {
        id: PeerId,
        muted: bool,
    },
    /// A measurement click arrived this many nanoseconds after it was sent.
    ClickDelay(u64),
}

pub struct Call {
    pipeline: gst::Pipeline,
    mic_tee: gst::Element,
    mixer: gst::Element,
    config: CallConfig,
    events: UnboundedSender<CallEvent>,
    peers: Mutex<HashMap<PeerId, Arc<Peer>>>,
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
}

struct SendBranch {
    bin: gst::Bin,
    tee_pad: gst::Pad,
}

struct RecvBranch {
    bin: gst::Bin,
    mixer_pad: gst::Pad,
}

impl Call {
    pub fn new(config: CallConfig, events: UnboundedSender<CallEvent>) -> Result<Self> {
        let capture = if config.test_tone {
            format!(
                "audiotestsrc is-live=true wave=silence samplesperbuffer=480 \
                 ! capsfilter name=clicks caps={RAW_AUDIO}"
            )
        } else {
            let processing = if config.raw_mic {
                String::new()
            } else {
                // Echo cancellation needs the playback signal, which only
                // exists when we actually play audio.
                format!(
                    "! webrtcdsp echo-cancel={} noise-suppression=true gain-control=true \
                     high-pass-filter=true",
                    !config.measure
                )
            };
            format!(
                "pulsesrc name=mic buffer-time=20000 latency-time=10000 \
                 ! audioconvert ! audioresample ! {RAW_AUDIO} {processing}"
            )
        };
        let playback = if config.measure {
            format!("appsink name=detector sync=true processing-deadline={SINK_DEADLINE_NS}")
        } else {
            let probe = if config.test_tone || config.raw_mic {
                ""
            } else {
                "webrtcechoprobe !"
            };
            format!(
                "{probe} audioconvert ! audioresample \
                 ! pulsesink name=speakers buffer-time=20000 latency-time=10000 \
                   processing-deadline={SINK_DEADLINE_NS}"
            )
        };
        let jitter_ns = u64::from(config.jitter_ms) * 1_000_000;
        let description = format!(
            "{capture} \
             ! opusenc audio-type=voice bitrate=64000 frame-size=10 inband-fec=true \
               packet-loss-percentage=5 \
             ! tee name=mic_tee allow-not-linked=true \
             audiomixer name=mixer force-live=true latency={MIXER_SLACK_NS} \
               min-upstream-latency={jitter_ns} \
             ! {RAW_AUDIO} ! {playback}"
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

        if let Some(device) = &config.input
            && !config.test_tone
        {
            by_name("mic")?.set_property("device", device);
        }
        if let Some(device) = &config.output
            && !config.measure
        {
            by_name("speakers")?.set_property("device", device);
        }
        if config.test_tone {
            install_click_writer(&by_name("clicks")?)?;
        }
        if config.measure {
            install_click_detector(&by_name("detector")?, events.clone())?;
        }

        let call = Self {
            mic_tee: by_name("mic_tee")?,
            mixer: by_name("mixer")?,
            pipeline,
            config,
            events,
            peers: Mutex::new(HashMap::new()),
        };
        call.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the audio pipeline (is the audio device available?)")?;
        Ok(call)
    }

    pub fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("pipelines always have a bus")
    }

    /// Newly added live elements change the pipeline's latency; this applies it.
    pub fn recalculate_latency(&self) {
        let _ = self.pipeline.recalculate_latency();
    }

    /// Starts a connection to `id`. Exactly one side must pass `we_offer`.
    pub fn add_peer(&self, id: PeerId, name: &str, we_offer: bool) -> Result<()> {
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(format!("peer-{id}"))
            .property_from_str("bundle-policy", "max-bundle")
            .property("latency", self.config.jitter_ms)
            .build()
            .context("cannot create webrtcbin")?;
        if let Some(stun) = &self.config.ice.stun {
            webrtc.set_property("stun-server", stun);
        }
        for turn in &self.config.ice.turn {
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
            let state =
                webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            let state = format!("{state:?}").to_lowercase();
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

        channel.connect_on_open(|channel| {
            let _ = channel.send_string_full(Some(&json!({ "muted": false }).to_string()));
        });
        let events = self.events.clone();
        let id = peer.id;
        channel.connect_on_message_string(move |_, message| {
            let parsed = message.and_then(|text| serde_json::from_str::<Value>(text).ok());
            if let Some(muted) = parsed.and_then(|msg| msg["muted"].as_bool()) {
                let _ = events.send(CallEvent::PeerMuted { id, muted });
            }
        });
        *peer.control.lock().unwrap() = Some(channel);
        Ok(())
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
    let bin = gst::parse::bin_from_description(
        &format!(
            "rtpopusdepay ! opusdec plc=true \
             ! audioconvert ! audioresample ! capsfilter caps={RAW_AUDIO}"
        ),
        true,
    )?;
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
    *peer.recv.lock().unwrap() = Some(RecvBranch { bin, mixer_pad });
    Ok(())
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
