//! Screen sharing: turning a picture source into one H.264 stream for all
//! viewers, and a received stream back into frames.
//!
//! ```text
//! source -> videorate -> videoscale -> nvh264enc -> h264parse -> [Encoder output]
//!
//! [Decoder input] -> h264parse -> nvh264dec -> videoconvert -> [frames]
//! ```
//!
//! Both run in pipelines of their own, apart from the call's. A pipeline has
//! one latency for everything in it, so an encoder or a decoder inside the
//! call's pipeline would delay the voice as well.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use flowerchat_protocol::PeerId;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use serde_json::{Value, json};
use tracing::{debug, warn};

/// The encoded video as it travels between the pipelines: whole frames with
/// start codes.
pub(crate) const H264: &str = "video/x-h264,stream-format=byte-stream,alignment=au";
/// The widest and tallest picture the H.264 encoder takes.
const MAX_SIDE: u32 = 4096;
/// Bits spent per pixel at 60 frames per second. 1080p60 comes to about
/// 8.7 Mbit/s and 4K60 to about 35.
const BITS_PER_PIXEL_AT_60: f64 = 0.07;
/// How many frames' worth of bits the encoder may spend on one frame. Low
/// keeps the stream even, so a large frame does not arrive late.
const FRAMES_OF_BURST: f64 = 2.0;
/// Least time between two keyframes that viewers ask for.
const KEYFRAME_SPACING_NS: u64 = 250_000_000;
/// Frame rate of the test picture.
const TEST_PICTURE_FPS: u32 = 60;
/// How often a screen that is not changing is sent again anyway. Someone
/// who starts watching needs a frame to start from.
const STILL_SCREEN_REFRESH_MS: i32 = 100;

/// What the person sharing chooses about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareSettings {
    /// Tallest picture to send. A taller source is scaled down to it; a
    /// smaller one is sent as it is. `None` always sends the source's size.
    pub max_height: Option<u32>,
    /// Most frames to send per second.
    pub fps: u32,
}

impl Default for ShareSettings {
    fn default() -> Self {
        Self {
            max_height: Some(1080),
            fps: 60,
        }
    }
}

/// Where the shared picture comes from.
#[derive(Debug, Clone)]
pub enum ShareSource {
    /// A moving test pattern of this size, standing in for a screen.
    TestPicture { width: u32, height: u32 },
    /// A screen or a window, as PipeWire delivers it.
    Screen(ScreenSource),
}

/// A PipeWire video stream of a screen or a window. The desktop's
/// ScreenCast portal hands these out once the user has picked what to share.
#[derive(Debug, Clone)]
pub struct ScreenSource {
    /// A connection to PipeWire that is allowed to see the stream. Without
    /// one, the ordinary connection is used, which only sees streams that
    /// are open to everyone.
    pub remote: Option<Arc<OwnedFd>>,
    /// The stream's node in PipeWire.
    pub node: u32,
}

/// A share as the room sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sharing {
    /// Size of the picture being sent.
    pub width: u32,
    pub height: u32,
    /// Frames per second the sharer asked for.
    pub fps: u32,
    /// Bitrate the encoder aims for.
    pub kbps: u32,
    /// Frames per second actually sent of late.
    pub sent_fps: f64,
    /// Time from a frame being captured to it being encoded.
    pub encode_ms: f64,
    /// Who the stream is being sent to.
    pub watchers: Vec<PeerId>,
}

impl Sharing {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
            "kbps": self.kbps,
            "sentFps": self.sent_fps,
            "encodeMs": self.encode_ms,
            "watchers": self.watchers,
        })
    }

    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        let number = |field: &str| value[field].as_u64().and_then(|n| u32::try_from(n).ok());
        Some(Self {
            width: number("width")?,
            height: number("height")?,
            fps: number("fps")?,
            kbps: number("kbps")?,
            sent_fps: value["sentFps"].as_f64().unwrap_or(0.0),
            encode_ms: value["encodeMs"].as_f64().unwrap_or(0.0),
            watchers: value["watchers"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default(),
        })
    }
}

/// Reads a size written as `1920x1080`.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// The size to send a `source`-sized picture at.
pub fn fit(source: (u32, u32), max_height: Option<u32>) -> (u32, u32) {
    let (width, height) = (source.0.max(2), source.1.max(2));
    let mut scale = 1.0_f64;
    if let Some(max) = max_height {
        scale = scale.min(f64::from(max) / f64::from(height));
    }
    scale = scale
        .min(f64::from(MAX_SIDE) / f64::from(width))
        .min(f64::from(MAX_SIDE) / f64::from(height));
    // The encoder works on even sizes only.
    if scale >= 1.0 {
        return (width & !1, height & !1);
    }
    let even = |side: u32| (((f64::from(side) * scale / 2.0).round() as u32) * 2).max(2);
    (even(width), even(height))
}

/// The bitrate for a picture of this size and rate. Half the frames do not
/// halve the bits: with more time between frames, more changes from one to
/// the next.
pub fn bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let fps = f64::from(fps.max(1));
    let bits_per_pixel = BITS_PER_PIXEL_AT_60 * (60.0 / fps).sqrt();
    let kbps = f64::from(width) * f64::from(height) * fps * bits_per_pixel / 1000.0;
    ((kbps / 100.0).round() as u32 * 100).max(300)
}

// --- Sending ----------------------------------------------------------------

/// What the encoder reports from GStreamer's threads.
pub(crate) enum EncoderEvent {
    /// An encoded frame, with the time it was captured as running time of
    /// the call's pipeline.
    Frame(gst::Buffer),
    /// The size or rate of what is sent has changed.
    Format,
    /// The source has ended (`None`) or something broke.
    Ended(Option<String>),
}

/// Encodes a picture source once, for everyone who watches.
pub(crate) struct Encoder {
    pipeline: gst::Pipeline,
    rate: gst::Element,
    size: gst::Element,
    encoder: gst::Element,
    /// Tells the encoder how many frames per second to expect.
    pace: gst::Element,
    state: Arc<Mutex<EncoderState>>,
    counters: Arc<EncoderCounters>,
    /// When the counters were last read.
    last_read: Mutex<Instant>,
    /// Kept open for as long as the screen is being read through it.
    _remote: Option<Arc<OwnedFd>>,
}

struct EncoderState {
    settings: ShareSettings,
    /// Size of what the source delivers, once known.
    source: Option<(u32, u32)>,
    /// Size and bitrate being sent.
    format: Option<(u32, u32, u32)>,
}

#[derive(Default)]
struct EncoderCounters {
    frames: AtomicU64,
    /// Sum of the time frames spent between capture and encoder output.
    latency_ns: AtomicU64,
}

/// What an [`Encoder`] has done since it was last asked.
pub(crate) struct EncoderStats {
    pub fps: f64,
    pub encode_ms: f64,
}

impl Encoder {
    /// Starts encoding `source`. `clock` and `base_time` are those of the
    /// pipeline the frames are headed for, so that their timestamps can be
    /// expressed in its terms.
    pub fn start(
        source: &ShareSource,
        settings: ShareSettings,
        clock: &gst::Clock,
        base_time: gst::ClockTime,
        events: impl Fn(EncoderEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let source_description = match source {
            ShareSource::TestPicture { width, height } => format!(
                "videotestsrc is-live=true pattern=smpte \
                 ! video/x-raw,format=BGRx,width={width},height={height},\
                   framerate={TEST_PICTURE_FPS}/1"
            ),
            // PipeWire only delivers a frame when the screen has changed.
            ShareSource::Screen(_) => format!(
                "pipewiresrc name=screen do-timestamp=true \
                   keepalive-time={STILL_SCREEN_REFRESH_MS}"
            ),
        };
        // The encoder sends a keyframe only when asked: a new viewer and a
        // viewer that lost data both ask, and nobody else pays for it.
        //
        // A screen delivers frames at no fixed rate, and says so. The
        // encoder shares out its bitrate by the rate it expects, so it is
        // told the most it can get.
        let description = format!(
            "{source_description} \
             ! videorate name=rate drop-only=true max-rate={} \
             ! capssetter name=pace join=true replace=false caps=\"{}\" \
             ! videoconvert n-threads=2 ! videoscale add-borders=false n-threads=4 \
             ! capsfilter name=size \
             ! nvh264enc name=encoder preset=p4 tune=ultra-low-latency rc-mode=cbr \
               zerolatency=true bframes=0 gop-size=-1 repeat-sequence-header=true \
               min-force-key-unit-interval={KEYFRAME_SPACING_NS} \
             ! video/x-h264,profile=high ! h264parse config-interval=-1 ! {H264} \
             ! appsink name=out sync=false",
            settings.fps,
            pace_caps(settings.fps)
        );
        debug!(%description, "building the encoder");
        let pipeline = gst::parse::launch(&description)
            .context("cannot build the video encoder")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("the video encoder is not a pipeline"))?;
        let by_name = |name: &str| {
            pipeline
                .by_name(name)
                .ok_or_else(|| anyhow!("encoder element {name} is missing"))
        };
        let (rate, size, encoder) = (by_name("rate")?, by_name("size")?, by_name("encoder")?);
        let pace = by_name("pace")?;
        let remote = match source {
            ShareSource::Screen(screen) => {
                let pipewire = by_name("screen")?;
                pipewire.set_property("path", screen.node.to_string());
                if let Some(remote) = &screen.remote {
                    pipewire.set_property("fd", remote.as_raw_fd());
                }
                screen.remote.clone()
            }
            ShareSource::TestPicture { .. } => None,
        };
        let state = Arc::new(Mutex::new(EncoderState {
            settings,
            source: None,
            format: None,
        }));
        let counters = Arc::new(EncoderCounters::default());
        let events = Arc::new(events);

        // The source says how large its picture is, also when that changes
        // later. Everything after it follows.
        let (weak_size, weak_encoder) = (size.downgrade(), encoder.downgrade());
        let (probe_state, probe_events) = (state.clone(), events.clone());
        rate.static_pad("sink")
            .ok_or_else(|| anyhow!("the frame rate limiter has no sink pad"))?
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                let Some(gst::EventView::Caps(caps)) = info.event().map(|event| event.view())
                else {
                    return gst::PadProbeReturn::Ok;
                };
                let Ok(video) = gst_video::VideoInfo::from_caps(caps.caps()) else {
                    return gst::PadProbeReturn::Ok;
                };
                if let (Some(size), Some(encoder)) = (weak_size.upgrade(), weak_encoder.upgrade()) {
                    let mut state = probe_state.lock().unwrap();
                    state.source = Some((video.width(), video.height()));
                    state.configure(&size, &encoder);
                    drop(state);
                    probe_events(EncoderEvent::Format);
                }
                gst::PadProbeReturn::Ok
            });

        // Both pipelines count time on the same clock, but from different
        // starting points.
        pipeline.use_clock(Some(clock));
        let sink = by_name("out")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("the encoder's output is not an appsink"))?;
        let (sink_counters, sink_events) = (counters.clone(), events.clone());
        let (sink_clock, weak_pipeline) = (clock.clone(), pipeline.downgrade());
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let mut buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    let own_base = weak_pipeline.upgrade().and_then(|p| p.base_time());
                    let captured = sample
                        .segment()
                        .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
                        .zip(buffer.pts())
                        .and_then(|(segment, pts)| segment.to_running_time(pts));
                    let (Some(own_base), Some(captured)) = (own_base, captured) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let captured_at = own_base + captured;
                    let age = sink_clock.time().saturating_sub(captured_at);
                    sink_counters.frames.fetch_add(1, Ordering::Relaxed);
                    sink_counters
                        .latency_ns
                        .fetch_add(age.nseconds(), Ordering::Relaxed);

                    let buffer_mut = buffer.make_mut();
                    buffer_mut.set_pts(captured_at.saturating_sub(base_time));
                    buffer_mut.set_dts(gst::ClockTime::NONE);
                    sink_events(EncoderEvent::Frame(buffer));
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let bus = pipeline.bus().expect("pipelines always have a bus");
        bus.set_sync_handler(move |_, message| {
            match message.view() {
                gst::MessageView::Error(err) => {
                    let source = err.src().map(|s| s.path_string()).unwrap_or_default();
                    events(EncoderEvent::Ended(Some(format!(
                        "screen sharing failed in {source}: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    ))));
                }
                gst::MessageView::Eos(_) => events(EncoderEvent::Ended(None)),
                _ => {}
            }
            gst::BusSyncReply::Drop
        });

        let this = Self {
            pipeline,
            rate,
            size,
            encoder,
            pace,
            state,
            counters,
            last_read: Mutex::new(Instant::now()),
            _remote: remote,
        };
        this.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the video encoder")?;
        Ok(this)
    }

    /// Changes the size or rate of what is sent, without a break.
    pub fn set_settings(&self, settings: ShareSettings) {
        let mut state = self.state.lock().unwrap();
        if state.settings == settings {
            return;
        }
        state.settings = settings;
        self.rate.set_property("max-rate", settings.fps as i32);
        self.pace.set_property("caps", pace_caps(settings.fps));
        // Makes the new pace take effect with the next frame, whether or
        // not anything else about the picture changes.
        if let Some(pad) = self.pace.static_pad("src") {
            pad.send_event(gst::event::Reconfigure::new());
        }
        state.configure(&self.size, &self.encoder);
    }

    /// The size of the picture being sent and the bitrate aimed for, once
    /// the source has said what it delivers.
    pub fn format(&self) -> Option<(u32, u32, u32)> {
        self.state.lock().unwrap().format
    }

    pub fn settings(&self) -> ShareSettings {
        self.state.lock().unwrap().settings
    }

    /// Makes the next frame one that can be decoded on its own.
    pub fn force_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(pad) = self.encoder.static_pad("src") {
            pad.send_event(event);
        }
    }

    /// What has been sent since this was last called.
    pub fn take_stats(&self) -> EncoderStats {
        let mut last_read = self.last_read.lock().unwrap();
        let seconds = last_read.elapsed().as_secs_f64().max(0.001);
        *last_read = Instant::now();
        let frames = self.counters.frames.swap(0, Ordering::Relaxed);
        let latency_ns = self.counters.latency_ns.swap(0, Ordering::Relaxed);
        EncoderStats {
            fps: frames as f64 / seconds,
            encode_ms: if frames == 0 {
                0.0
            } else {
                latency_ns as f64 / frames as f64 / 1e6
            },
        }
    }
}

/// The caps that tell the encoder to expect `fps` frames per second.
fn pace_caps(fps: u32) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(fps.max(1) as i32, 1))
        .build()
}

impl EncoderState {
    /// Sets the pipeline up for the source and settings at hand.
    fn configure(&mut self, size: &gst::Element, encoder: &gst::Element) {
        let Some(source) = self.source else {
            return;
        };
        let (width, height) = fit(source, self.settings.max_height);
        let kbps = bitrate_kbps(width, height, self.settings.fps);
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        size.set_property("caps", &caps);
        encoder.set_property("bitrate", kbps);
        let burst = f64::from(kbps) * FRAMES_OF_BURST / f64::from(self.settings.fps.max(1));
        encoder.set_property("vbv-buffer-size", burst.round() as u32);
        debug!(?source, width, height, kbps, "encoder configured");
        self.format = Some((width, height, kbps));
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
    }
}

// --- Receiving --------------------------------------------------------------

/// One decoded frame of someone's screen.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pixels: gst::MappedBuffer<gst::buffer::Readable>,
}

impl VideoFrame {
    /// The picture as rows of RGBA pixels, top row first, without padding.
    pub fn rgba(&self) -> &[u8] {
        &self.pixels
    }
}

/// Receives the frames of the screens being watched, on one of GStreamer's
/// threads. Whatever it does holds up the frame after it.
pub type FrameSink = Arc<dyn Fn(PeerId, VideoFrame) + Send + Sync>;

/// How watching someone's screen is going.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchStats {
    /// Size of the picture the sharer sends. Zero until the first frame.
    pub width: u32,
    pub height: u32,
    /// Frames per second shown of late.
    pub fps: f64,
    /// Share of the decoded frames that were not shown, because the frames
    /// before them were still being prepared.
    pub dropped_percent: f64,
    /// Time from a frame's arrival to it being ready to show.
    pub decode_ms: f64,
}

/// Decodes one sharer's stream into frames.
pub(crate) struct Decoder {
    pipeline: gst::Pipeline,
    input: gst_app::AppSrc,
    /// Sets the size frames are delivered at.
    size: gst::Element,
    /// Whether frames stay on the graphics card until they are delivered.
    on_gpu: bool,
    state: Arc<Mutex<DecoderState>>,
    counters: Arc<DecoderCounters>,
    last_read: Mutex<Instant>,
}

struct DecoderState {
    /// Size of the picture in the stream, once known.
    stream: Option<(u32, u32)>,
    /// Tallest the frames are wanted, or zero for as they come.
    max_height: u32,
}

impl DecoderState {
    /// Sets the pipeline up to deliver frames at the size wanted.
    fn configure(&self, size: &gst::Element, on_gpu: bool) {
        let mut caps = gst::Caps::builder("video/x-raw").field("format", "RGBA");
        if let Some((width, height)) = self.stream
            && self.max_height > 0
            && height > self.max_height
        {
            let scale = f64::from(self.max_height) / f64::from(height);
            caps = caps
                .field("width", (f64::from(width) * scale).round() as i32)
                .field("height", self.max_height as i32)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        }
        let caps = if on_gpu {
            caps.features(["memory:GLMemory"]).build()
        } else {
            caps.build()
        };
        size.set_property("caps", caps);
    }
}

/// Whether frames can be converted and scaled on the graphics card on this
/// system. Tried once, with a single small frame.
fn gpu_conversion_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        let attempt = || -> Result<bool> {
            let pipeline = gst::parse::launch(
                "videotestsrc num-buffers=1 ! video/x-raw,format=NV12,width=64,height=64 \
                 ! glupload ! glcolorconvert ! glcolorscale \
                 ! video/x-raw(memory:GLMemory),format=RGBA,width=32,height=32 \
                 ! gldownload ! video/x-raw,format=RGBA ! fakesink",
            )?;
            pipeline.set_state(gst::State::Playing)?;
            let bus = pipeline.bus().context("no bus")?;
            let ended = [gst::MessageType::Eos, gst::MessageType::Error];
            let message = bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &ended);
            let _ = pipeline.set_state(gst::State::Null);
            Ok(message.is_some_and(|m| m.type_() == gst::MessageType::Eos))
        };
        let works = attempt().unwrap_or(false);
        if !works {
            warn!("frames cannot be prepared on the graphics card; the processor does it");
        }
        works
    })
}

#[derive(Default)]
struct DecoderCounters {
    decoded: AtomicU64,
    shown: AtomicU64,
    /// Sum of the time the shown frames took from arrival to being shown.
    latency_ns: AtomicU64,
    /// Width and height of the last frame, packed into one number.
    size: AtomicU64,
    /// When the frames now inside the decoder arrived, by timestamp.
    arrivals: Mutex<VecDeque<(gst::ClockTime, Instant)>>,
}

/// How many frames can be inside the decoder at once before the oldest are
/// forgotten. Only the measuring of the delay depends on it.
const ARRIVALS_KEPT: usize = 64;

impl Decoder {
    /// `max_height` is the tallest the frames are wanted, or zero for the
    /// size the sharer sends.
    pub fn start(from: PeerId, frames: FrameSink, max_height: u32) -> Result<Self> {
        // Turning what the decoder puts out into pixels for display, at the
        // size they are shown at, is quick on the graphics card and slow
        // on the processor.
        let on_gpu = gpu_conversion_works();
        let prepare = if on_gpu {
            "glupload ! glcolorconvert ! glcolorscale ! capsfilter name=size \
             ! gldownload ! video/x-raw,format=RGBA"
        } else {
            "videoconvertscale n-threads=4 ! capsfilter name=size"
        };
        // Encoded frames cannot be skipped, because the ones after them
        // build on them. Decoded ones can, so when preparing frames for
        // display falls behind, frames are dropped ahead of that step.
        let description = format!(
            "appsrc name=in is-live=true format=time caps={H264} \
             ! h264parse ! nvh264dec name=decoder \
             ! queue leaky=downstream max-size-buffers=2 max-size-time=0 max-size-bytes=0 \
             ! {prepare} \
             ! appsink name=out sync=false max-buffers=2 drop=true"
        );
        let pipeline = gst::parse::launch(&description)
            .context("cannot build the video decoder")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("the video decoder is not a pipeline"))?;
        let by_name = |name: &str| {
            pipeline
                .by_name(name)
                .ok_or_else(|| anyhow!("decoder element {name} is missing"))
        };
        let input = by_name("in")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow!("the decoder's input is not an appsrc"))?;
        let counters = Arc::new(DecoderCounters::default());
        let size = by_name("size")?;
        let state = Arc::new(Mutex::new(DecoderState {
            stream: None,
            max_height,
        }));
        state.lock().unwrap().configure(&size, on_gpu);

        let decoded = counters.clone();
        let (probe_state, weak_size) = (state.clone(), size.downgrade());
        by_name("decoder")?
            .static_pad("src")
            .ok_or_else(|| anyhow!("the decoder has no src pad"))?
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    // The stream says how large its picture is, also when
                    // the sharer changes that.
                    if let Some(gst::EventView::Caps(caps)) = info.event().map(|e| e.view()) {
                        if let Ok(video) = gst_video::VideoInfo::from_caps(caps.caps())
                            && let Some(size) = weak_size.upgrade()
                        {
                            let mut state = probe_state.lock().unwrap();
                            state.stream = Some((video.width(), video.height()));
                            state.configure(&size, on_gpu);
                        }
                    } else if info.buffer().is_some() {
                        decoded.decoded.fetch_add(1, Ordering::Relaxed);
                    }
                    gst::PadProbeReturn::Ok
                },
            );

        let shown = counters.clone();
        by_name("out")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("the decoder's output is not an appsink"))?
            .set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let video = sample
                            .caps()
                            .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
                            .ok_or(gst::FlowError::NotNegotiated)?;
                        let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                        let (width, height) = (video.width(), video.height());
                        shown.note_shown(buffer.pts(), width, height);
                        let pixels = buffer
                            .into_mapped_buffer_readable()
                            .map_err(|_| gst::FlowError::Error)?;
                        if pixels.len() == width as usize * height as usize * 4 {
                            frames(
                                from,
                                VideoFrame {
                                    width,
                                    height,
                                    pixels,
                                },
                            );
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

        let bus = pipeline.bus().expect("pipelines always have a bus");
        bus.set_sync_handler(move |_, message| {
            if let gst::MessageView::Error(err) = message.view() {
                warn!(
                    peer = from,
                    "cannot decode the shared screen: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
            gst::BusSyncReply::Drop
        });
        pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the video decoder")?;
        Ok(Self {
            pipeline,
            input,
            size,
            on_gpu,
            state,
            counters,
            last_read: Mutex::new(Instant::now()),
        })
    }

    /// Changes the tallest the frames are wanted, or with zero asks for
    /// them at the size the sharer sends.
    pub fn set_max_height(&self, max_height: u32) {
        let mut state = self.state.lock().unwrap();
        if state.max_height != max_height {
            state.max_height = max_height;
            state.configure(&self.size, self.on_gpu);
        }
    }

    /// Hands the decoder the next encoded frame.
    pub fn push(&self, buffer: gst::Buffer) {
        if let Some(pts) = buffer.pts() {
            let mut arrivals = self.counters.arrivals.lock().unwrap();
            if arrivals.len() == ARRIVALS_KEPT {
                arrivals.pop_front();
            }
            arrivals.push_back((pts, Instant::now()));
        }
        let _ = self.input.push_buffer(buffer);
    }

    /// How it has gone since this was last called.
    pub fn take_stats(&self) -> WatchStats {
        let mut last_read = self.last_read.lock().unwrap();
        let seconds = last_read.elapsed().as_secs_f64().max(0.001);
        *last_read = Instant::now();
        let decoded = self.counters.decoded.swap(0, Ordering::Relaxed);
        let shown = self.counters.shown.swap(0, Ordering::Relaxed);
        let latency_ns = self.counters.latency_ns.swap(0, Ordering::Relaxed);
        let shown_any = self.counters.size.load(Ordering::Relaxed) != 0;
        let (width, height) = self
            .state
            .lock()
            .unwrap()
            .stream
            .filter(|_| shown_any)
            .unwrap_or_default();
        WatchStats {
            width,
            height,
            fps: shown as f64 / seconds,
            dropped_percent: if decoded > shown {
                100.0 * (decoded - shown) as f64 / decoded as f64
            } else {
                0.0
            },
            decode_ms: if shown == 0 {
                0.0
            } else {
                latency_ns as f64 / shown as f64 / 1e6
            },
        }
    }
}

impl DecoderCounters {
    fn note_shown(&self, pts: Option<gst::ClockTime>, width: u32, height: u32) {
        self.shown.fetch_add(1, Ordering::Relaxed);
        self.size.store(
            u64::from(width) << 32 | u64::from(height),
            Ordering::Relaxed,
        );
        let Some(pts) = pts else {
            return;
        };
        let mut arrivals = self.arrivals.lock().unwrap();
        // Frames come out in the order they went in, so everything older
        // than this one was dropped along the way.
        while let Some((arrived_pts, arrived)) = arrivals.pop_front() {
            if arrived_pts == pts {
                self.latency_ns
                    .fetch_add(arrived.elapsed().as_nanos() as u64, Ordering::Relaxed);
                break;
            }
            if arrived_pts > pts {
                arrivals.push_front((arrived_pts, arrived));
                break;
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_only_ever_scaled_down() {
        assert_eq!(fit((2560, 1440), Some(1080)), (1920, 1080));
        assert_eq!(fit((2560, 1440), Some(2160)), (2560, 1440));
        assert_eq!(fit((2560, 1440), None), (2560, 1440));
        assert_eq!(fit((3440, 1440), Some(720)), (1720, 720));
    }

    #[test]
    fn sizes_stay_within_what_the_encoder_takes() {
        assert_eq!(fit((5120, 1440), None), (4096, 1152));
        assert_eq!(fit((1281, 721), None), (1280, 720));
        let (width, height) = fit((1366, 768), Some(720));
        assert_eq!((width % 2, height), (0, 720));
    }

    #[test]
    fn sizes_are_read_as_written() {
        assert_eq!(parse_size("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_size("1080p"), None);
    }

    #[test]
    fn bitrate_follows_size_and_rate() {
        assert_eq!(bitrate_kbps(1920, 1080, 60), 8700);
        assert_eq!(bitrate_kbps(3840, 2160, 60), 34800);
        assert_eq!(bitrate_kbps(1920, 1080, 30), 6200);
    }

    #[test]
    fn a_share_survives_the_trip_as_json() {
        let sharing = Sharing {
            width: 1920,
            height: 1080,
            fps: 60,
            kbps: 8700,
            sent_fps: 59.5,
            encode_ms: 4.25,
            watchers: vec![2, 5],
        };
        assert_eq!(Sharing::from_json(&sharing.to_json()), Some(sharing));
        assert_eq!(Sharing::from_json(&Value::Null), None);
    }
}
