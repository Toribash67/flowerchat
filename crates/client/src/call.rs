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
//!
//! A shared screen passes through the same pipeline, already encoded on the
//! way out and still encoded on the way in ([`crate::video`] has the rest):
//!
//! ```text
//! encoder ~> appsrc -> tee -+-> [gate -> rtph264pay -> queue] -> webrtcbin (peer A)
//!                           +-> [gate -> rtph264pay -> queue] -> webrtcbin (peer B)
//!
//! webrtcbin (peer A) -> [rtph264depay -> appsink] ~> decoder
//! ```
//!
//! Every connection is set up to carry video in both directions from the
//! start, so that sharing and watching need no new negotiation. A gate lets
//! the stream through to a peer only while that peer watches.
//!
//! All of this exists once per [`Codec`], each with a stream of its own in
//! every connection. Only the one the sharer has picked carries anything,
//! and picking another moves the frames over to its stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use flowerchat_protocol::PeerId;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_sdp as gst_sdp;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, trace, warn};

use crate::audio::{
    AudioSettings, LEVEL_INTERVAL_NS, MAX_JITTER_MS, RAW_AUDIO, SPEAKERS, capture_bin, level_db,
    unique_probe_name,
};
use crate::ice::IceConfig;
use crate::latency::{ClickDetector, write_clicks};
use crate::sound::{STEREO, SoundSource, StreamSound};
use crate::video::{
    Codec, Decoder, Encoder, EncoderEvent, FrameSink, ShareSettings, ShareSource, Sharing,
    WatchStats,
};

/// Payload type we propose for Opus when we make the offer. An answer has to
/// reuse whatever the offer picked instead.
const OPUS_PAYLOAD_TYPE: i32 = 111;
/// The same for video in each codec. The numbers between are left for the
/// streams that carry lost packets again.
const fn video_payload_type(codec: Codec) -> i32 {
    match codec {
        Codec::H264 => 96,
        Codec::Hevc => 98,
    }
}
/// Least time between two keyframes asked for because frames arrive in a
/// codec the viewer is not decoding.
const KEYFRAME_REQUEST_SPACING: Duration = Duration::from_millis(500);
/// The same for the sound that goes with a shared screen, which is Opus
/// like the voice and told apart from it by this.
const STREAM_SOUND_PAYLOAD_TYPE: i32 = 112;
/// Ticks per second of video timestamps in RTP.
const VIDEO_CLOCK_RATE: i32 = 90_000;
/// The bitrate each connection is told it carries, which sets how soon it
/// may send feedback: at this rate, within a few milliseconds.
const FEEDBACK_BANDWIDTH: f64 = 4_000_000.0;
/// Largest RTP packet we send. Smaller than most networks allow, to leave
/// room for what tunnels and relays add.
const VIDEO_MTU: u32 = 1200;
/// How much encoded video may wait for one peer's connection before the
/// oldest is dropped.
const VIDEO_BACKLOG_BYTES: u32 = 4_000_000;
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
/// The same for the sound of a peer's shared screen.
const STREAM_LEVEL_PREFIX: &str = "level-stream-";

/// States of the gate between our share and one peer.
const GATE_CLOSED: u8 = 0;
/// Opens at the next frame that can be decoded on its own.
const GATE_AT_KEYFRAME: u8 = 1;
const GATE_OPEN: u8 = 2;

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
    /// Where the frames of watched screens go. Without it they are decoded
    /// and counted, but not shown.
    pub frames: Option<FrameSink>,
    /// For testing: the share, from 0 to 1, of arriving packets to lose, as
    /// a poor network would.
    pub incoming_loss: f64,
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
    /// What we are sharing as of now, or that we no longer are.
    OwnShare(Option<Sharing>),
    /// Our share has ended on its own (`None`) or broken down. The call
    /// expects [`Call::stop_share`] in reply.
    ShareEnded(Option<String>),
    /// The size of what we share has become known or has changed. The call
    /// expects [`Call::announce_share`] in reply.
    ShareFormat,
    /// A peer wants our share, or no longer does. The call expects
    /// [`Call::set_watcher`] in reply.
    WatchRequest {
        from: PeerId,
        watch: bool,
    },
    /// What a peer is sharing as of now, or that it no longer is.
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
    /// scale.
    StreamLevel {
        id: PeerId,
        db: f32,
    },
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
    /// The same for the peer's shared screen, while we watch it.
    pub video_loss_percent: Option<f64>,
    pub video_receiving_kbps: Option<f64>,
    /// Bitrate of our shared screen towards this peer, while it watches.
    pub video_sending_kbps: Option<f64>,
    /// Bitrate of the sound of the peer's shared screen, and of ours.
    pub sound_receiving_kbps: Option<f64>,
    pub sound_sending_kbps: Option<f64>,
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
    /// Where the encoded frames of our share enter the pipeline and are
    /// handed out, for each codec.
    video: Vec<VideoEntry>,
    /// Where the sound of our share, already encoded, enters the pipeline
    /// and is handed out.
    sound_src: gst_app::AppSrc,
    sound_tee: gst::Element,
    /// Whether incoming sound is timed and not played.
    measure: bool,
    /// Our share, while there is one.
    share: Arc<Mutex<Option<Share>>>,
    /// What we last told the peers about our share. Shared with the control
    /// channels, which repeat it to peers that connect later.
    announced: Arc<Mutex<Option<Value>>>,
    frames: FrameSink,
    incoming_loss: f64,
}

/// The way into the pipeline for our share in one codec.
#[derive(Clone)]
struct VideoEntry {
    codec: Codec,
    src: gst_app::AppSrc,
    tee: gst::Element,
}

struct Share {
    encoder: Encoder,
    /// Frame rate and encoding time as last measured.
    sent_fps: f64,
    encode_ms: f64,
    /// Whether the share is a test picture, whose sound is a test tone.
    test: bool,
    /// The sound that goes with it, if any is sent.
    sound: Option<StreamSound>,
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
    counters: Mutex<StatsHistory>,
    /// One per codec the connection carries.
    video_send: Mutex<Vec<(Codec, SendBranch)>>,
    /// Whether our share goes out to this peer: one of the `GATE_` states.
    gate: Arc<AtomicU8>,
    video_recv: Mutex<Vec<gst::Bin>>,
    /// Decodes this peer's share while we watch it.
    viewer: Arc<Mutex<Option<Decoder>>>,
    /// The codec this peer last said it shares in.
    shared_codec: Arc<Mutex<Codec>>,
    /// The payload type this connection uses for video in each codec, as
    /// far as that is settled.
    video_payloads: Arc<Mutex<Vec<(Codec, i32)>>>,
    /// The tallest this peer's share is shown, or zero for any size.
    view_height: AtomicU32,
    sound_send: Mutex<Option<SendBranch>>,
    sound_recv: Mutex<Option<SoundRecv>>,
    /// The payload type this connection uses for the sound of shared
    /// screens, or -1 if it carries none.
    sound_payload: Arc<AtomicI32>,
    /// Playback gain for the sound of this peer's share.
    sound_volume: Mutex<f64>,
}

/// What plays the sound of a peer's shared screen.
struct SoundRecv {
    bin: gst::Bin,
    /// Where it enters the mix, and its volume control. Absent when the
    /// sound is only measured.
    mixer_pad: Option<gst::Pad>,
    volume: Option<gst::Element>,
}

/// The counters of the previous stats report, per kind of media.
#[derive(Default)]
struct StatsHistory {
    audio: Option<Counters>,
    video: Option<Counters>,
    /// When the video we watch and the video we send were last counted, in
    /// milliseconds, and how many bytes they had come to.
    video_in: Option<(f64, f64)>,
    video_out: Option<(f64, f64)>,
}

/// Cumulative counters at the time of a stats report.
#[derive(Clone, Copy)]
struct Counters {
    packets_received: f64,
    packets_lost: f64,
}

impl Peer {
    fn video_payload(&self, codec: Codec) -> Option<i32> {
        let payloads = self.video_payloads.lock().unwrap();
        payloads
            .iter()
            .find_map(|(known, payload)| (*known == codec).then_some(*payload))
    }
}

struct SendBranch {
    bin: gst::Bin,
    tee: gst::Element,
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
            frames,
            incoming_loss,
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
                 ! {SPEAKERS} name=speakers processing-deadline={SINK_DEADLINE_NS}"
            )
        };
        let jitter_ns = u64::from(audio.jitter_ms) * 1_000_000;
        // Voices are mono, but the sound of a shared screen is not, and
        // both are played through the same mix. The click detector takes
        // one channel.
        let mixed = if measure { RAW_AUDIO } else { STEREO };
        let video_entries: String = Codec::ALL
            .iter()
            .map(|codec| {
                format!(
                    "appsrc name=video_src_{id} is-live=true format=time caps={} \
                     ! tee name=video_tee_{id} allow-not-linked=true ",
                    codec.caps(),
                    id = codec.id()
                )
            })
            .collect();
        let description = format!(
            "volume name=mic_gate \
             ! level name={MIC_LEVEL} interval={LEVEL_INTERVAL_NS} post-messages=true \
             ! opusenc audio-type=voice bitrate=64000 frame-size=10 inband-fec=true \
               packet-loss-percentage=5 \
             ! tee name=mic_tee allow-not-linked=true \
             audiomixer name=mixer force-live=true latency={MIXER_SLACK_NS} \
               min-upstream-latency={jitter_ns} \
             ! {mixed} ! volume name=out_gate ! {playback} \
             {video_entries} \
             appsrc name=sound_src is-live=true format=time \
             ! tee name=sound_tee allow-not-linked=true"
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

        let share: Arc<Mutex<Option<Share>>> = Arc::default();
        let mut video = Vec::new();
        for codec in Codec::ALL {
            let src = by_name(&format!("video_src_{}", codec.id()))?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| anyhow!("the video entry is not an appsrc"))?;
            // A viewer that has lost data asks for a frame it can start
            // over from. The request travels up the pipeline as an event
            // and ends at the entry, from where the encoder has to be told
            // by hand.
            let weak_share = Arc::downgrade(&share);
            src.static_pad("src")
                .ok_or_else(|| anyhow!("the video entry has no src pad"))?
                .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                    if let Some(event) = info.event()
                        && gst_video::ForceKeyUnitEvent::is(event)
                        && let Some(share) = weak_share.upgrade()
                        && let Some(share) = &*share.lock().unwrap()
                    {
                        share.encoder.force_keyframe();
                    }
                    gst::PadProbeReturn::Ok
                });
            video.push(VideoEntry {
                codec,
                src,
                tee: by_name(&format!("video_tee_{}", codec.id()))?,
            });
        }

        let call = Self {
            video,
            sound_src: by_name("sound_src")?
                .downcast::<gst_app::AppSrc>()
                .map_err(|_| anyhow!("the sound entry is not an appsrc"))?,
            sound_tee: by_name("sound_tee")?,
            measure,
            share,
            announced: Arc::default(),
            frames: frames.unwrap_or_else(|| Arc::new(|_, _| {})),
            incoming_loss,
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
                    } else if let Some(id) = source
                        .strip_prefix(STREAM_LEVEL_PREFIX)
                        .and_then(|id| id.parse().ok())
                    {
                        let _ = self.events.send(CallEvent::StreamLevel { id, db });
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

    /// Sets how loud the sound of one peer's shared screen is played, 1.0
    /// being unchanged.
    pub fn set_stream_volume(&self, id: PeerId, volume: f64) {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return;
        };
        *peer.sound_volume.lock().unwrap() = volume;
        if let Some(recv) = &*peer.sound_recv.lock().unwrap()
            && let Some(control) = &recv.volume
        {
            control.set_property("volume", volume);
        }
    }

    /// Sends a chat message to everyone we are connected to.
    pub fn send_chat(&self, text: &str) {
        self.broadcast(&json!({ "chat": text }));
    }

    /// Starts sharing `source` with whoever asks to watch.
    pub fn start_share(&self, source: &ShareSource, settings: ShareSettings) -> Result<()> {
        self.stop_share();
        // The clock is chosen while the pipeline starts, which may still be
        // under way.
        let _ = self.pipeline.state(gst::ClockTime::from_seconds(2));
        let (Some(clock), Some(base_time)) = (self.pipeline.clock(), self.pipeline.base_time())
        else {
            bail!("the call has not started yet");
        };
        let entries = self.video.clone();
        let events = self.events.clone();
        let encoder = Encoder::start(
            source,
            settings,
            &clock,
            base_time,
            move |event| match event {
                EncoderEvent::Frame(codec, buffer) => {
                    if let Some(entry) = entries.iter().find(|entry| entry.codec == codec) {
                        let _ = entry.src.push_buffer(buffer);
                    }
                }
                EncoderEvent::Format => {
                    let _ = events.send(CallEvent::ShareFormat);
                }
                EncoderEvent::Ended(reason) => {
                    let _ = events.send(CallEvent::ShareEnded(reason));
                }
            },
        )?;
        let test = matches!(source, ShareSource::TestPicture { .. });
        let sound = settings.sound.then(|| self.attach_sound(test)).flatten();
        *self.share.lock().unwrap() = Some(Share {
            encoder,
            sent_fps: 0.0,
            encode_ms: 0.0,
            test,
            sound,
        });
        Ok(())
    }

    /// Starts capturing the sound that goes with our share. A share is
    /// worth having without it, so failing is noted and not passed on.
    fn attach_sound(&self, test: bool) -> Option<StreamSound> {
        let source = if test {
            SoundSource::Tone
        } else {
            SoundSource::Programs
        };
        let (clock, base_time) = (self.pipeline.clock()?, self.pipeline.base_time()?);
        let sound_src = self.sound_src.clone();
        StreamSound::start(source, &clock, base_time, move |packet| {
            let _ = sound_src.push_sample(&packet);
        })
        .inspect_err(|err| warn!("sharing without sound: {err:#}"))
        .ok()
    }

    /// Changes the size, rate, bitrate or codec of what we share, or
    /// whether sound goes with it, without a break.
    pub fn set_share_settings(&self, settings: ShareSettings) {
        if let Some(share) = &mut *self.share.lock().unwrap() {
            share.encoder.set_settings(settings);
            if settings.sound && share.sound.is_none() {
                share.sound = self.attach_sound(share.test);
            } else if !settings.sound {
                share.sound = None;
            }
        }
        self.announce_share();
    }

    /// Stops sharing, for everyone.
    pub fn stop_share(&self) {
        let Some(share) = self.share.lock().unwrap().take() else {
            return;
        };
        for peer in self.peers.lock().unwrap().values() {
            peer.gate.store(GATE_CLOSED, Ordering::SeqCst);
        }

        // Stopping the encoder waits for its threads, so it happens with no
        // lock held that they might want.
        drop(share);
        self.announce_share();
    }

    /// Starts or stops sending our share to `id`, at that peer's request.
    pub fn set_watcher(&self, id: PeerId, watch: bool) {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return;
        };
        {
            let share = self.share.lock().unwrap();
            let Some(share) = &*share else {
                return;
            };
            if watch {
                // The peer can make nothing of the stream until a frame
                // arrives that does not build on earlier ones.
                peer.gate.store(GATE_AT_KEYFRAME, Ordering::SeqCst);
                share.encoder.force_keyframe();
            } else {
                peer.gate.store(GATE_CLOSED, Ordering::SeqCst);
            }
        }
        self.announce_share();
    }

    /// Tells the peers, and whoever listens to our events, what we share
    /// as of now.
    pub fn announce_share(&self) {
        let format = self.share.lock().unwrap().as_ref().and_then(|share| {
            let (width, height, kbps) = share.encoder.format()?;
            let (source_width, source_height) = share.encoder.source()?;
            Some(Sharing {
                width,
                height,
                source_width,
                source_height,
                fps: share.encoder.settings().fps,
                codec: share.encoder.settings().codec,
                kbps,
                sent_fps: share.sent_fps,
                encode_ms: share.encode_ms,
                sound: share.sound.is_some(),
                watchers: Vec::new(),
            })
        });
        let sharing = format.map(|mut sharing| {
            let peers = self.peers.lock().unwrap();
            sharing.watchers = peers
                .values()
                .filter(|peer| peer.gate.load(Ordering::SeqCst) != GATE_CLOSED)
                .map(|peer| peer.id)
                .collect();
            sharing.watchers.sort_unstable();
            sharing
        });
        let message = sharing.as_ref().map(Sharing::to_json);
        let was_announced = {
            let mut announced = self.announced.lock().unwrap();
            let was = announced.is_some();
            *announced = message.clone();
            was
        };
        if message.is_some() || was_announced {
            self.broadcast(&json!({ "sharing": message }));
            let _ = self.events.send(CallEvent::OwnShare(sharing));
        }
    }

    /// Says how tall, in pixels, the screen that `id` shares is shown at
    /// most, so that its frames are not prepared any larger than that. Zero
    /// asks for them at the size they are sent.
    pub fn set_view_height(&self, id: PeerId, max_height: u32) {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return;
        };
        peer.view_height.store(max_height, Ordering::SeqCst);
        if let Some(viewer) = &*peer.viewer.lock().unwrap() {
            viewer.set_max_height(max_height);
        }
    }

    /// Starts or stops watching what `id` shares.
    pub fn watch(&self, id: PeerId, watch: bool) -> Result<()> {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return Ok(());
        };
        let previous = if watch {
            let max_height = peer.view_height.load(Ordering::SeqCst);
            let codec = *peer.shared_codec.lock().unwrap();
            let decoder = Decoder::start(id, self.frames.clone(), max_height, codec)?;
            peer.viewer.lock().unwrap().replace(decoder)
        } else {
            peer.viewer.lock().unwrap().take()
        };
        // Stopping a decoder takes a moment; nothing else should wait.
        drop(previous);
        send_to(&peer, &json!({ "watch": watch }));
        Ok(())
    }

    /// Lets go of what was set up to watch `id`, after it stopped sharing.
    pub fn peer_stopped_sharing(&self, id: PeerId) {
        let Some(peer) = self.peers.lock().unwrap().get(&id).cloned() else {
            return;
        };
        let viewer = peer.viewer.lock().unwrap().take();
        drop(viewer);
    }

    /// Measures how sharing and watching have gone since the last call, and
    /// reports it.
    fn measure_video(&self) {
        let sharing = {
            let mut share = self.share.lock().unwrap();
            share.as_mut().map(|share| {
                let stats = share.encoder.take_stats();
                share.sent_fps = stats.fps;
                share.encode_ms = stats.encode_ms;
            })
        };
        if sharing.is_some() {
            self.announce_share();
        }
        for peer in self.peers.lock().unwrap().values() {
            if let Some(viewer) = &*peer.viewer.lock().unwrap() {
                let stats = viewer.take_stats();
                let _ = self.events.send(CallEvent::Watching { id: peer.id, stats });
            }
        }
    }

    /// Asks every connection for its statistics; they arrive as events.
    pub fn request_stats(&self) {
        self.measure_video();
        let sharing_in = self
            .share
            .lock()
            .unwrap()
            .as_ref()
            .map(|share| share.encoder.settings().codec);
        for peer in self.peers.lock().unwrap().values() {
            let events = self.events.clone();
            let weak_peer = Arc::downgrade(peer);
            let promise = gst::Promise::with_change_func(move |reply| {
                let (Ok(Some(reply)), Some(peer)) = (reply, weak_peer.upgrade()) else {
                    return;
                };
                trace!(peer = peer.id, "raw stats: {reply:?}");
                let watching_in = peer.viewer.lock().unwrap().as_ref().map(Decoder::codec);
                let payloads = StreamPayloads {
                    sound: peer.sound_payload.load(Ordering::SeqCst),
                    video_out: sharing_in.and_then(|codec| peer.video_payload(codec)),
                    video_in: watching_in.and_then(|codec| peer.video_payload(codec)),
                };
                let stats = read_stats(reply, &mut peer.counters.lock().unwrap(), payloads);
                let _ = events.send(CallEvent::PeerStats { id: peer.id, stats });
            });
            peer.webrtc
                .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        }
    }

    fn broadcast(&self, message: &Value) {
        for peer in self.peers.lock().unwrap().values() {
            send_to(peer, message);
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
            counters: Mutex::default(),
            video_send: Mutex::default(),
            gate: Arc::new(AtomicU8::new(GATE_CLOSED)),
            video_recv: Mutex::default(),
            viewer: Arc::default(),
            shared_codec: Arc::default(),
            video_payloads: Arc::default(),
            view_height: AtomicU32::new(0),
            sound_send: Mutex::new(None),
            sound_recv: Mutex::new(None),
            sound_payload: Arc::new(AtomicI32::new(-1)),
            sound_volume: Mutex::new(1.0),
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
        let measure = self.measure;
        let weak_peer = Arc::downgrade(&peer);
        webrtc.connect_pad_added(move |_, pad| {
            let (Some(pipeline), Some(peer)) = (pipeline.upgrade(), weak_peer.upgrade()) else {
                return;
            };
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            let media = caps
                .structure(0)
                .and_then(|s| s.get::<String>("media").ok());
            let payload = caps.structure(0).and_then(|s| s.get::<i32>("payload").ok());
            if media.as_deref() == Some("video") {
                let codec = caps
                    .structure(0)
                    .and_then(|s| s.get::<&str>("encoding-name").ok())
                    .and_then(Codec::from_rtp_name);
                let attached = match codec {
                    Some(codec) => attach_video_recv_branch(&pipeline, &peer, pad, codec),
                    None => Err(anyhow!("it is in no codec we know: {caps}")),
                };
                if let Err(err) = attached {
                    warn!(peer = peer.id, "cannot take in incoming video: {err:#}");
                }
            } else if payload == Some(peer.sound_payload.load(Ordering::SeqCst)) {
                if let Err(err) = attach_sound_recv_branch(&pipeline, &mixer, &peer, pad, measure) {
                    warn!(peer = peer.id, "cannot play a stream's sound: {err:#}");
                }
            } else if let Err(err) = attach_recv_branch(&pipeline, &mixer, &peer, pad) {
                warn!(peer = peer.id, "cannot play incoming audio: {err:#}");
            }
        });

        tune_feedback(&webrtc, peer.video_payloads.clone());
        if self.incoming_loss > 0.0 {
            lose_incoming_packets(&webrtc, self.incoming_loss);
        }
        self.pipeline.add(&webrtc)?;
        // Data channels can only be created once the element has left NULL.
        webrtc.set_state(gst::State::Ready)?;
        self.open_control_channel(&peer)?;
        webrtc.sync_state_with_parent()?;

        // Only now is the connection given anything to send. The microphone
        // is running already, and a connection that is handed a packet
        // before it has been started turns it away, after which the branch
        // that brought it sends nothing more: the peer never hears us. A
        // packet comes every 10 ms, so that happened now and then, and
        // more often on a busy machine.
        if we_offer {
            // As the offerer we pick the payload types. As the answerer we
            // have to wait for the offer to learn them.
            self.attach_send_branch(&peer, OPUS_PAYLOAD_TYPE)?;
            for entry in &self.video {
                let payload_type = video_payload_type(entry.codec);
                self.attach_video_send_branch(&peer, entry, payload_type)?;
            }
            self.attach_sound_send_branch(&peer, STREAM_SOUND_PAYLOAD_TYPE)?;
        }

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
        let was_watching = peer.gate.swap(GATE_CLOSED, Ordering::SeqCst) != GATE_CLOSED;
        let pipeline = self.pipeline.clone();
        let mixer = self.mixer.clone();
        let video = std::mem::take(&mut *peer.video_send.lock().unwrap());
        let branches: Vec<SendBranch> = [
            peer.send.lock().unwrap().take(),
            peer.sound_send.lock().unwrap().take(),
        ]
        .into_iter()
        .flatten()
        .chain(video.into_iter().map(|(_, branch)| branch))
        .collect();
        let tee_pads = branches.iter().map(|b| b.tee_pad.clone()).collect();
        // Stopping elements can block, so it happens off the thread that
        // finds the last branch cut off.
        unlink_when_idle(
            tee_pads,
            Box::new(move || {
                pipeline.call_async(move |pipeline| teardown(pipeline, &mixer, &peer, branches));
            }),
        );
        if was_watching {
            self.announce_share();
        }
    }

    pub fn shutdown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    fn on_offer(&self, peer: &Arc<Peer>, sdp: gst_sdp::SDPMessage) -> Result<()> {
        if peer.awaiting_answer.load(Ordering::SeqCst) {
            debug!(peer = peer.id, "ignoring an offer that collided with ours");
            return Ok(());
        }
        // The first audio in an offer is the voice. A second, if there is
        // one, is the sound of shared screens.
        let audio = payload_types(&sdp, "audio", "opus");
        if peer.send.lock().unwrap().is_none() {
            let payload_type = *audio
                .first()
                .ok_or_else(|| anyhow!("the offer does not include Opus audio"))?;
            self.attach_send_branch(peer, payload_type)?;
        }
        // Not everyone offers video: the browser test page does not. Nor
        // need an offer have every codec.
        for entry in &self.video {
            let attached = peer.video_payload(entry.codec).is_some();
            if !attached
                && let Some(payload_type) =
                    payload_types(&sdp, "video", entry.codec.rtp_name()).first()
            {
                self.attach_video_send_branch(peer, entry, *payload_type)?;
            }
        }
        if peer.sound_send.lock().unwrap().is_none()
            && let Some(payload_type) = audio.get(1)
        {
            self.attach_sound_send_branch(peer, *payload_type)?;
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

        *peer.send.lock().unwrap() = Some(SendBranch {
            bin,
            tee: self.mic_tee.clone(),
            tee_pad,
        });
        Ok(())
    }

    /// Prepares this peer's connection for the sound of our share. Nothing
    /// is sent until the peer asks to watch.
    fn attach_sound_send_branch(&self, peer: &Peer, payload_type: i32) -> Result<()> {
        peer.sound_payload.store(payload_type, Ordering::SeqCst);
        let ssrc = gst::glib::random_int();
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "OPUS")
            .field("clock-rate", 48_000i32)
            .field("encoding-params", "2")
            .field("sprop-stereo", "1")
            .field("payload", payload_type)
            .field("ssrc", ssrc)
            .build();
        let bin = gst::parse::bin_from_description(
            &format!(
                "queue leaky=downstream max-size-buffers=20 max-size-time=0 max-size-bytes=0 \
                 ! rtpopuspay pt={payload_type} ssrc={ssrc} ! capsfilter caps=\"{caps}\""
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
            .ok_or_else(|| anyhow!("sound send branch has no src pad"))?
            .link(&webrtc_sink)?;

        bin.sync_state_with_parent()?;
        let tee_pad = self
            .sound_tee
            .request_pad_simple("src_%u")
            .ok_or_else(|| anyhow!("tee refused a src pad"))?;
        // The sound goes to those the picture goes to.
        let gate = peer.gate.clone();
        tee_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if gate.load(Ordering::SeqCst) == GATE_CLOSED {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        });
        tee_pad.link(
            &bin.static_pad("sink")
                .ok_or_else(|| anyhow!("sound send branch has no sink pad"))?,
        )?;

        *peer.sound_send.lock().unwrap() = Some(SendBranch {
            bin,
            tee: self.sound_tee.clone(),
            tee_pad,
        });
        Ok(())
    }

    /// Prepares this peer's connection for our share in one codec. Nothing
    /// is sent until the peer asks to watch.
    fn attach_video_send_branch(
        &self,
        peer: &Peer,
        entry: &VideoEntry,
        payload_type: i32,
    ) -> Result<()> {
        // The stream is given its identifier here and not when the first
        // frame is sent, so that it is part of what gets negotiated. The
        // other side needs it to tell which stream to ask again for packets
        // that went missing.
        let codec = entry.codec;
        peer.video_payloads
            .lock()
            .unwrap()
            .push((codec, payload_type));
        let payloader = match codec {
            Codec::H264 => "rtph264pay",
            Codec::Hevc => "rtph265pay",
        };
        let ssrc = gst::glib::random_int();
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", codec.rtp_name())
            .field("clock-rate", VIDEO_CLOCK_RATE)
            .field("payload", payload_type)
            .field("ssrc", ssrc)
            .field("rtcp-fb-nack-pli", true)
            .build();
        // The queue comes after the packets are numbered, so that what it
        // drops when the connection cannot keep up shows as loss at the
        // other end, which then asks for a fresh start.
        let bin = gst::parse::bin_from_description(
            &format!(
                "{payloader} pt={payload_type} ssrc={ssrc} mtu={VIDEO_MTU} config-interval=-1 \
                   aggregate-mode=zero-latency \
                 ! queue leaky=downstream max-size-bytes={VIDEO_BACKLOG_BYTES} \
                   max-size-buffers=0 max-size-time=0 \
                 ! capsfilter caps=\"{caps}\""
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
        // Packets that go missing are asked for again. Only when that does
        // not help in time does the viewer need a fresh keyframe.
        webrtc_sink
            .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver")
            .set_property("do-nack", true);
        bin.static_pad("src")
            .ok_or_else(|| anyhow!("video send branch has no src pad"))?
            .link(&webrtc_sink)?;

        bin.sync_state_with_parent()?;
        let tee_pad = entry
            .tee
            .request_pad_simple("src_%u")
            .ok_or_else(|| anyhow!("tee refused a src pad"))?;
        let gate = peer.gate.clone();
        tee_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            match gate.load(Ordering::SeqCst) {
                GATE_OPEN => gst::PadProbeReturn::Ok,
                GATE_AT_KEYFRAME
                    if info
                        .buffer()
                        .is_some_and(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT)) =>
                {
                    gate.store(GATE_OPEN, Ordering::SeqCst);
                    gst::PadProbeReturn::Ok
                }
                _ => gst::PadProbeReturn::Drop,
            }
        });
        tee_pad.link(
            &bin.static_pad("sink")
                .ok_or_else(|| anyhow!("video send branch has no sink pad"))?,
        )?;

        let branch = SendBranch {
            bin,
            tee: entry.tee.clone(),
            tee_pad,
        };
        peer.video_send.lock().unwrap().push((codec, branch));
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
        let announced = self.announced.clone();
        channel.connect_on_open(move |channel| {
            let mut state = json!({ "muted": muted.load(Ordering::SeqCst) });
            if let Some(sharing) = &*announced.lock().unwrap() {
                state["sharing"] = sharing.clone();
            }
            let _ = channel.send_string_full(Some(&state.to_string()));
        });
        let events = self.events.clone();
        let id = peer.id;
        let shared_codec = peer.shared_codec.clone();
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
            if let Some(watch) = msg["watch"].as_bool() {
                let _ = events.send(CallEvent::WatchRequest { from: id, watch });
            }
            if let Some(sharing) = msg.get("sharing") {
                let sharing = Sharing::from_json(sharing);
                if let Some(sharing) = &sharing {
                    *shared_codec.lock().unwrap() = sharing.codec;
                }
                let _ = events.send(CallEvent::PeerSharing { id, sharing });
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

/// Decodes the sound of a peer's shared screen and feeds it to the mixer,
/// or with `measure`, which plays nothing, only reports how loud it is.
fn attach_sound_recv_branch(
    pipeline: &gst::Pipeline,
    mixer: &gst::Element,
    peer: &Peer,
    webrtc_src: &gst::Pad,
    measure: bool,
) -> Result<()> {
    let level = format!(
        "level name={STREAM_LEVEL_PREFIX}{} interval={LEVEL_INTERVAL_NS} post-messages=true",
        peer.id
    );
    let description = if measure {
        format!(
            "rtpopusdepay ! opusdec plc=true ! audioconvert ! {level} \
             ! fakesink sync=false async=false"
        )
    } else {
        format!(
            "rtpopusdepay ! opusdec plc=true \
             ! audioconvert ! audioresample ! capsfilter caps={STEREO} \
             ! {level} ! volume name=volume"
        )
    };
    let bin = gst::parse::bin_from_description(&description, true)?;
    let volume = bin.by_name("volume");
    if let Some(volume) = &volume {
        volume.set_property("volume", *peer.sound_volume.lock().unwrap());
    }
    pipeline.add(&bin)?;
    let mixer_pad = match bin.static_pad("src") {
        Some(src) => {
            let mixer_pad = mixer
                .request_pad_simple("sink_%u")
                .ok_or_else(|| anyhow!("mixer refused a sink pad"))?;
            src.link(&mixer_pad)?;
            Some(mixer_pad)
        }
        None => None,
    };
    bin.sync_state_with_parent()?;
    webrtc_src.link(
        &bin.static_pad("sink")
            .ok_or_else(|| anyhow!("sound receive branch has no sink pad"))?,
    )?;
    debug!(peer = peer.id, name = %peer.name, "receiving a stream's sound");
    *peer.sound_recv.lock().unwrap() = Some(SoundRecv {
        bin,
        mixer_pad,
        volume,
    });
    Ok(())
}

/// Decodes nothing: takes a peer's shared screen in one codec out of the
/// pipeline, still encoded, and hands it to whatever decoder is watching.
fn attach_video_recv_branch(
    pipeline: &gst::Pipeline,
    peer: &Peer,
    webrtc_src: &gst::Pad,
    codec: Codec,
) -> Result<()> {
    let depayloader = match codec {
        Codec::H264 => "rtph264depay",
        Codec::Hevc => "rtph265depay",
    };
    // After a loss, frames are held back until one arrives that can be
    // decoded on its own, and the sharer is asked for it.
    let bin = gst::parse::bin_from_description(
        &format!(
            "{depayloader} request-keyframe=true wait-for-keyframe=true ! {} \
             ! appsink name=out sync=false async=false",
            codec.caps()
        ),
        true,
    )?;
    let viewer = peer.viewer.clone();
    let id = peer.id;
    let mut last_request: Option<Instant> = None;
    bin.by_name("out")
        .and_then(|out| out.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow!("video receive branch has no exit"))?
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let Some(buffer) = sample.buffer_owned() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let mut viewer = viewer.lock().unwrap();
                    let Some(decoder) = &mut *viewer else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    if decoder.codec() != codec {
                        // The sharer has changed codec, and this is the
                        // stream it changed to or the one it changed from.
                        // A keyframe settles it: a stream that has ended
                        // sends none. Should the new stream's first have
                        // been taken for the old one's last, asking for
                        // another puts it right.
                        if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                            let due = last_request
                                .is_none_or(|at| at.elapsed() >= KEYFRAME_REQUEST_SPACING);
                            if due && let Some(pad) = sink.static_pad("sink") {
                                last_request = Some(Instant::now());
                                let request = gst_video::UpstreamForceKeyUnitEvent::builder()
                                    .all_headers(true)
                                    .build();
                                pad.push_event(request);
                            }
                            return Ok(gst::FlowSuccess::Ok);
                        }
                        match decoder.for_codec(codec) {
                            Ok(new) => *decoder = new,
                            Err(err) => {
                                warn!(peer = id, "cannot follow the change of codec: {err:#}");
                                return Ok(gst::FlowSuccess::Ok);
                            }
                        }
                    }
                    decoder.push(buffer);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    pipeline.add(&bin)?;
    bin.sync_state_with_parent()?;
    webrtc_src.link(
        &bin.static_pad("sink")
            .ok_or_else(|| anyhow!("video receive branch has no sink pad"))?,
    )?;
    debug!(peer = peer.id, name = %peer.name, codec = codec.label(), "receiving video");
    peer.video_recv.lock().unwrap().push(bin);
    Ok(())
}

/// Sees to it that a connection can ask for lost video packets again, and
/// can do so at once.
///
/// `video_payloads` holds the payload types the connection uses for video,
/// once they are settled.
fn tune_feedback(webrtc: &gst::Element, video_payloads: Arc<Mutex<Vec<(Codec, i32)>>>) {
    let Some(bin) = webrtc.downcast_ref::<gst::Bin>() else {
        return;
    };
    // The parts concerned are created later, when the connection is
    // negotiated and when media first arrives.
    bin.connect_deep_element_added(move |_, _, element| {
        let Some(factory) = element.factory() else {
            return;
        };
        match factory.name().as_str() {
            // How often a connection may send feedback follows from how
            // much data it is said to carry. Left to measure that itself, it
            // takes seconds to notice that a stream has started, and until
            // then most requests for lost packets come too late. Routine
            // reports are not sent any more often for this.
            "rtpsession" => element.set_property("bandwidth", FEEDBACK_BANDWIDTH),
            // webrtcbin turns retransmission on or off for everything a
            // connection receives at once, by what the last of its streams
            // wants. Here it is the video that wants it. Voice and stream
            // sound are not set up for it, and asking for them would only
            // use up the feedback that the video needs. So each stream is
            // set on its own, by what its first packet shows it to be.
            "rtpjitterbuffer" => {
                // Left alone, the buffer works out when the next packet
                // is due and gives it up for lost when it does not come.
                // A share that nobody watches, or that has moved to
                // another codec, sends nothing for a while; the first
                // packet after that would then be thrown away as too
                // late, and with it what a decoder needs to start. Packets
                // are still missed, and asked for again, as soon as a
                // later one shows the gap.
                element.set_property("rtx-next-seqnum", false);
                let weak = element.downgrade();
                let video_payloads = video_payloads.clone();
                let Some(pad) = element.static_pad("sink") else {
                    return;
                };
                pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    let payload_type = info
                        .buffer()
                        .and_then(|buffer| buffer.map_readable().ok())
                        .and_then(|packet| packet.get(1).map(|byte| i32::from(byte & 0x7f)));
                    if let Some(jitterbuffer) = weak.upgrade() {
                        let payloads = video_payloads.lock().unwrap();
                        let is_video = payloads
                            .iter()
                            .any(|(_, payload)| Some(*payload) == payload_type);
                        jitterbuffer.set_property("do-retransmission", is_video);
                    }
                    gst::PadProbeReturn::Remove
                });
            }
            _ => {}
        }
    });
}

/// For testing: makes a connection lose `share` of the packets that arrive,
/// whatever they carry.
fn lose_incoming_packets(webrtc: &gst::Element, share: f64) {
    let Some(bin) = webrtc.downcast_ref::<gst::Bin>() else {
        return;
    };
    // The element that receives from the network is created later, when
    // the connection is negotiated.
    bin.connect_deep_element_added(move |_, _, element| {
        if element.factory().is_none_or(|f| f.name() != "nicesrc") {
            return;
        }
        let Some(pad) = element.static_pad("src") else {
            return;
        };
        let mut state = u64::from(gst::glib::random_int()) << 32 | 1;
        let random = Mutex::new(move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        });
        pad.add_probe(gst::PadProbeType::BUFFER_LIST, move |_, info| {
            if let Some(gst::PadProbeData::BufferList(list)) = &mut info.data {
                let mut random = random.lock().unwrap();
                let list = list.make_mut();
                for index in (0..list.len()).rev() {
                    if random() < share {
                        list.remove(index..index + 1);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    });
}

/// Sends a message on a peer's control channel, if it is open.
fn send_to(peer: &Peer, message: &Value) {
    if let Some(channel) = &*peer.control.lock().unwrap()
        && channel.ready_state() == gst_webrtc::WebRTCDataChannelState::Open
    {
        let _ = channel.send_string_full(Some(&message.to_string()));
    }
}

/// The share of packets lost since the previous report, which `previous`
/// holds and which is replaced for the next one.
fn loss_since(object: &gst::StructureRef, previous: &mut Option<Counters>) -> Option<f64> {
    // The count beside `packets-received` only includes what the receive
    // buffer gave up waiting for, which misses most losses. The RTP session
    // keeps the real one.
    let source = object.get::<gst::Structure>("gst-rtpsource-stats").ok();
    let lost = source
        .as_ref()
        .and_then(|source| number(source, "packets-lost"))
        .or_else(|| number(object, "packets-lost"));
    let now = Counters {
        packets_received: number(object, "packets-received").unwrap_or(0.0),
        packets_lost: lost.unwrap_or(0.0).max(0.0),
    };
    let before = previous.replace(now)?;
    let lost = (now.packets_lost - before.packets_lost).max(0.0);
    let expected = now.packets_received - before.packets_received + lost;
    (expected > 0.0).then(|| 100.0 * lost / expected)
}

/// The payload types of the streams of a connection that statistics are
/// read for.
struct StreamPayloads {
    /// The sound of shared screens, which is audio like the voice and must
    /// not be taken for it.
    sound: i32,
    /// The video we send and the video we watch, in the codec each is in
    /// at the moment. The streams of the other codecs carry nothing, or
    /// figures from before a change of codec.
    video_out: Option<i32>,
    video_in: Option<i32>,
}

/// Turns webrtcbin's statistics into the figures we show. `previous` holds
/// the counters from the last report and is updated for the next one.
///
/// The report is a structure with one nested structure per statistics
/// object, named after its kind (`inbound-rtp`, `candidate-pair`, ...).
///
/// `payloads` says which of the connection's streams the figures are
/// wanted for.
fn read_stats(
    stats: &gst::StructureRef,
    previous: &mut StatsHistory,
    payloads: StreamPayloads,
) -> PeerStats {
    let mut out = PeerStats::default();
    let entry = |id: &str| stats.get::<gst::Structure>(id).ok();
    let text = |s: &gst::StructureRef, field: &str| s.get::<String>(field).ok();

    for (_, value) in stats.iter() {
        let Ok(object) = value.get::<gst::Structure>() else {
            continue;
        };
        // webrtcbin's own `kind` field says "audio" for incoming video, so
        // the codec is asked instead.
        let codec = text(&object, "codec-id").and_then(|id| entry(&id));
        let mime_type = codec.as_ref().and_then(|codec| text(codec, "mime-type"));
        let payload_type = codec
            .as_ref()
            .and_then(|codec| number(codec, "payload-type"));
        let carries = |payload: Option<i32>| payload.map(f64::from) == payload_type;
        let is_sound = carries(Some(payloads.sound));
        let kind = match &mime_type {
            Some(mime_type) => mime_type.split('/').next().map(str::to_owned),
            None => text(&object, "kind"),
        };
        let (is_audio, is_video) = (
            kind.as_deref() == Some("audio"),
            kind.as_deref() == Some("video"),
        );
        match object.name().as_str() {
            "inbound-rtp" if is_sound => out.sound_receiving_kbps = source_kbps(&object),
            "outbound-rtp" if is_sound => out.sound_sending_kbps = source_kbps(&object),
            "remote-inbound-rtp" if is_sound => {}
            "inbound-rtp" if is_audio => {
                out.jitter_ms = number(&object, "jitter").map(|seconds| seconds * 1000.0);
                out.receiving_kbps = source_kbps(&object);
                out.loss_percent = loss_since(&object, &mut previous.audio);
            }
            "inbound-rtp" if is_video && carries(payloads.video_in) => {
                out.video_receiving_kbps =
                    kbps_since(&object, "bytes-received", &mut previous.video_in);
                out.video_loss_percent = loss_since(&object, &mut previous.video);
            }
            "outbound-rtp" if is_audio => out.sending_kbps = source_kbps(&object),
            "outbound-rtp" if is_video && carries(payloads.video_out) => {
                out.video_sending_kbps = kbps_since(&object, "bytes-sent", &mut previous.video_out);
            }
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

/// The bitrate of a stream's contents since the previous report, by the
/// count of bytes in `field`. `previous` holds that report's count and is
/// updated for the next one.
///
/// The RTP session's own figure is an average over many seconds, which
/// suits a voice. A shared screen changes its bitrate when the sharer says
/// so, and the figure should follow.
fn kbps_since(
    object: &gst::StructureRef,
    field: &str,
    previous: &mut Option<(f64, f64)>,
) -> Option<f64> {
    let now = (number(object, "timestamp")?, number(object, field)?);
    let before = previous.replace(now)?;
    let (milliseconds, bytes) = (now.0 - before.0, now.1 - before.1);
    // A count that has gone down is that of another stream: the sharer
    // has changed codec.
    (milliseconds > 0.0 && bytes >= 0.0).then(|| bytes * 8.0 / milliseconds)
}

/// Cuts each of `tee_pads` off from what it feeds, at a moment when nothing
/// is passing through it, and calls `done` after the last.
///
/// A branch that is shut down while still linked returns an error to its
/// tee, which stops the stream for every other branch too.
fn unlink_when_idle(mut tee_pads: Vec<gst::Pad>, done: Box<dyn FnOnce() + Send>) {
    let Some(tee_pad) = tee_pads.pop() else {
        return done();
    };
    let pending = Mutex::new(Some((tee_pads, done)));
    tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
        if let Some((rest, done)) = pending.lock().unwrap().take() {
            if let Some(sink) = pad.peer() {
                let _ = pad.unlink(&sink);
            }
            unlink_when_idle(rest, done);
        }
        gst::PadProbeReturn::Remove
    });
}

/// Stops and removes everything that belonged to one peer. The send
/// branches must already be unlinked from their tees.
fn teardown(pipeline: &gst::Pipeline, mixer: &gst::Element, peer: &Peer, send: Vec<SendBranch>) {
    if let Some(channel) = peer.control.lock().unwrap().take() {
        channel.close();
    }
    let viewer = peer.viewer.lock().unwrap().take();
    drop(viewer);
    // Stopping webrtcbin first ends the flow into the receive branches.
    let _ = peer.webrtc.set_state(gst::State::Null);
    for send in send {
        let _ = send.bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&send.bin);
        send.tee.release_request_pad(&send.tee_pad);
    }
    if let Some(recv) = peer.recv.lock().unwrap().take() {
        let _ = recv.bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&recv.bin);
        mixer.release_request_pad(&recv.mixer_pad);
    }
    for bin in peer.video_recv.lock().unwrap().drain(..) {
        let _ = bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&bin);
    }
    if let Some(recv) = peer.sound_recv.lock().unwrap().take() {
        let _ = recv.bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&recv.bin);
        if let Some(mixer_pad) = &recv.mixer_pad {
            mixer.release_request_pad(mixer_pad);
        }
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

/// The payload type an offer assigns to `encoding` in each of its `kind`
/// media ("audio" or "video") that has it, in the order of the offer.
fn payload_types(sdp: &gst_sdp::SDPMessageRef, kind: &str, encoding: &str) -> Vec<i32> {
    let in_media = |media: &gst_sdp::SDPMediaRef| {
        media.formats().find_map(|format| {
            let payload_type = format.parse::<i32>().ok()?;
            let caps = media.caps_from_media(payload_type)?;
            let name = caps.structure(0)?.get::<&str>("encoding-name").ok()?;
            name.eq_ignore_ascii_case(encoding).then_some(payload_type)
        })
    };
    sdp.medias()
        .filter(|media| media.media() == Some(kind))
        .filter_map(in_media)
        .collect()
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
