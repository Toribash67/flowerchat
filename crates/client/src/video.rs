//! Screen sharing: turning a picture source into one encoded stream for all
//! viewers, and a received stream back into frames.
//!
//! ```text
//! source -> videorate -> videoscale -> {nvh264enc -> h264parse} -> [Encoder output]
//!
//! [Decoder input] -> h264parse -> nvh264dec -> videoconvert -> [frames]
//! ```
//!
//! Both run in pipelines of their own, apart from the call's. A pipeline has
//! one latency for everything in it, so an encoder or a decoder inside the
//! call's pipeline would delay the voice as well.
//!
//! The part in braces is replaced when the sharer picks another [`Codec`],
//! and the decoder with it at the other end.

use std::collections::VecDeque;
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

/// Bits H.264 spends per pixel at 60 frames per second, unless the sharer
/// sets a bitrate. 1080p60 comes to about 8.7 Mbit/s and 4K60 to about 35.
const BITS_PER_PIXEL_AT_60: f64 = 0.07;
/// The least and the most a sharer can set the bitrate to, in kbit/s.
pub const MIN_KBPS: u32 = 300;
pub const MAX_KBPS: u32 = 100_000;
/// How many frames' worth of bits the encoder may spend on one frame. Low
/// keeps the stream even, so a large frame does not arrive late.
const FRAMES_OF_BURST: f64 = 2.0;
/// Least time between two keyframes that viewers ask for.
const KEYFRAME_SPACING_NS: u64 = 250_000_000;
/// Frame rate of the test picture.
const TEST_PICTURE_FPS: u32 = 60;

/// How the picture is compressed on its way to the viewers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Understood by everything, and the lightest to encode and decode.
    #[default]
    H264,
    /// The same picture in fewer bits, or a better one in as many.
    Hevc,
}

impl Codec {
    pub const ALL: [Codec; 2] = [Codec::H264, Codec::Hevc];

    /// The name people know it by.
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        }
    }

    /// The name it has in messages between clients, in saved settings and
    /// on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|codec| codec.id() == id)
    }

    /// Whether this machine can encode it.
    pub fn can_encode(self) -> bool {
        gst::ElementFactory::find(self.encoder()).is_some()
    }

    /// The name it has in RTP.
    pub(crate) fn rtp_name(self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::Hevc => "H265",
        }
    }

    pub(crate) fn from_rtp_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|codec| codec.rtp_name().eq_ignore_ascii_case(name))
    }

    /// The encoded video as it travels between the pipelines: whole frames
    /// with start codes.
    pub(crate) fn caps(self) -> &'static str {
        match self {
            Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            Codec::Hevc => "video/x-h265,stream-format=byte-stream,alignment=au",
        }
    }

    fn of_caps(caps: &gst::CapsRef) -> Option<Self> {
        let name = caps.structure(0)?.name();
        Self::ALL
            .into_iter()
            .find(|codec| codec.caps().starts_with(name.as_str()))
    }

    #[cfg(not(windows))]
    fn encoder(self) -> &'static str {
        match self {
            Codec::H264 => "nvh264enc",
            Codec::Hevc => "nvh265enc",
        }
    }

    /// On Windows, the encoder that takes frames as Direct3D hands them
    /// out, which is how a captured screen arrives.
    #[cfg(windows)]
    fn encoder(self) -> &'static str {
        match self {
            Codec::H264 => "nvd3d11h264enc",
            Codec::Hevc => "nvd3d11h265enc",
        }
    }

    /// The elements that turn frames into the encoded stream. The encoder
    /// is named `encoder`.
    ///
    /// It sends a keyframe only when asked: a new viewer and a viewer that
    /// lost data both ask, and nobody else pays for it.
    ///
    /// On Linux, frames are copied to the graphics card ahead of the
    /// encoder ([`ENCODER_INPUT`]), which would otherwise do that itself and then hold on to the captured
    /// frame until it has encoded it, on a thread of its own. A frame of a
    /// screen belongs to PipeWire, and `pipewiresrc` (as of 1.6) locks up
    /// for good when one thread hands a frame back at the moment the
    /// capturing thread lets go of the frame it keeps for a still screen:
    /// each then holds the lock the other waits for. With the copy made
    /// here, on the capturing thread, no other thread ever holds one.
    fn encoder_description(self) -> String {
        let (profile, parser) = match self {
            Codec::H264 => ("video/x-h264,profile=high", "h264parse"),
            Codec::Hevc => ("video/x-h265,profile=main", "h265parse"),
        };
        format!(
            "{ENCODER_INPUT}{} name=encoder preset=p4 tune=ultra-low-latency rc-mode=cbr \
               zerolatency=true bframes=0 gop-size=-1 repeat-sequence-header=true \
               min-force-key-unit-interval={KEYFRAME_SPACING_NS} \
             ! {profile} ! {parser} config-interval=-1 ! capsfilter caps={}",
            self.encoder(),
            self.caps()
        )
    }

    /// The elements that turn the encoded stream into frames. The last is
    /// named `decoder`.
    fn decoder_description(self) -> &'static str {
        match self {
            Codec::H264 => "h264parse ! nvh264dec name=decoder",
            Codec::Hevc => "h265parse ! nvh265dec name=decoder",
        }
    }

    /// The widest and tallest picture its encoder takes.
    fn max_side(self) -> u32 {
        match self {
            Codec::H264 => 4096,
            Codec::Hevc => 8192,
        }
    }

    /// The share of H.264's bits it needs for a picture as good.
    fn bits_needed(self) -> f64 {
        match self {
            Codec::H264 => 1.0,
            Codec::Hevc => 0.75,
        }
    }
}

/// What the person sharing chooses about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareSettings {
    /// Tallest picture to send. A taller source is scaled down to it; a
    /// smaller one is sent as it is. `None` always sends the source's size.
    pub max_height: Option<u32>,
    /// Most frames to send per second.
    pub fps: u32,
    /// Whether to send along what the machine plays, the call left out.
    pub sound: bool,
    pub codec: Codec,
    /// The bitrate to aim for, in kbit/s. `None` follows the size and
    /// frame rate of the picture, as [`bitrate_kbps`] has it.
    pub kbps: Option<u32>,
}

impl Default for ShareSettings {
    fn default() -> Self {
        Self {
            max_height: Some(1080),
            fps: 60,
            sound: true,
            codec: Codec::default(),
            kbps: None,
        }
    }
}

/// What stands ahead of the encoder in its stage: see
/// [`Codec::encoder_description`].
#[cfg(not(windows))]
const ENCODER_INPUT: &str = "cudaupload ! ";
#[cfg(windows)]
const ENCODER_INPUT: &str = "";
/// The elements that give frames the size and the format they are encoded
/// at.
#[cfg(not(windows))]
const SIZING: &str = "videoconvert n-threads=2 ! videoscale add-borders=false n-threads=4";
// A captured screen is on the graphics card already, and stays there all
// the way into the encoder. A test picture is taken there first.
#[cfg(windows)]
const SIZING: &str = "d3d11upload ! d3d11convert add-borders=false";
/// The kind of memory frames are in once sized, on Windows.
#[cfg(windows)]
const SIZED_MEMORY: &str = "memory:D3D11Memory";

/// Makes Windows keep time to the millisecond in this process, for as long
/// as it lives.
///
/// A source of frames waits for the moment each frame is due. Left alone,
/// Windows only wakes a waiting thread every 15.6 ms, so a frame was taken
/// up to that much late: 7 ms on average, and unevenly, at 60 frames per
/// second.
#[cfg(windows)]
struct FineTimers;

#[cfg(windows)]
impl FineTimers {
    const MILLISECONDS: u32 = 1;

    fn new() -> Self {
        // Counted by Windows, so several of these can live at once.
        unsafe { windows_sys::Win32::Media::timeBeginPeriod(Self::MILLISECONDS) };
        Self
    }
}

#[cfg(windows)]
impl Drop for FineTimers {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Media::timeEndPeriod(Self::MILLISECONDS) };
    }
}

/// Where the shared picture comes from.
#[derive(Debug, Clone)]
pub enum ShareSource {
    /// A moving test pattern of this size, standing in for a screen.
    TestPicture { width: u32, height: u32 },
    /// A screen or a window, as the system delivers it.
    Screen(ScreenSource),
}

/// A PipeWire video stream of a screen or a window. The desktop's
/// ScreenCast portal hands these out once the user has picked what to share.
#[cfg(not(windows))]
#[derive(Debug, Clone)]
pub struct ScreenSource {
    /// A connection to PipeWire that is allowed to see the stream. Without
    /// one, the ordinary connection is used, which only sees streams that
    /// are open to everyone.
    pub remote: Option<Arc<std::os::fd::OwnedFd>>,
    /// The stream's node in PipeWire.
    pub node: u32,
}

#[cfg(not(windows))]
impl ScreenSource {
    /// How often a screen that is not changing is sent again anyway.
    /// Someone who starts watching needs a frame to start from.
    const STILL_SCREEN_REFRESH_MS: i32 = 100;

    /// The element that captures it, named `screen`, and whatever has to
    /// follow it. `fps` is the most frames per second that are wanted.
    fn description(&self, _fps: u32) -> String {
        // PipeWire only delivers a frame when the screen has changed.
        format!(
            "pipewiresrc name=screen do-timestamp=true keepalive-time={}",
            Self::STILL_SCREEN_REFRESH_MS
        )
    }

    /// Points the capturing element at this screen.
    fn apply(&self, screen: &gst::Element) {
        use std::os::fd::AsRawFd;
        screen.set_property("path", self.node.to_string());
        if let Some(remote) = &self.remote {
            screen.set_property("fd", remote.as_raw_fd());
        }
    }
}

/// A monitor or a window on Windows, which lets any program look at either.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSource {
    /// The monitor Windows calls the main one.
    PrimaryMonitor,
    /// A monitor by its place in the graphics card's list, from zero.
    MonitorNumber(u32),
    /// A monitor by its handle (`HMONITOR`).
    Monitor(u64),
    /// A window by its handle (`HWND`).
    Window(u64),
}

#[cfg(windows)]
impl ScreenSource {
    /// The element that captures it, named `screen`, and whatever has to
    /// follow it. `fps` is the most frames per second that are wanted.
    ///
    /// The capture delivers frames at a steady rate, changed or not, and
    /// has to be told which: the filter named `capture_rate` does that.
    fn description(&self, fps: u32) -> String {
        format!(
            "d3d11screencapturesrc name=screen show-cursor=true do-timestamp=true \
             ! capsfilter name=capture_rate caps=\"{}\"",
            capture_caps(fps)
        )
    }

    /// Points the capturing element at this screen.
    fn apply(&self, screen: &gst::Element) {
        match *self {
            ScreenSource::PrimaryMonitor => {}
            ScreenSource::MonitorNumber(number) => {
                screen.set_property("monitor-index", number as i32)
            }
            ScreenSource::Monitor(handle) => screen.set_property("monitor-handle", handle),
            // Only the newer of the two ways to capture knows of windows.
            ScreenSource::Window(handle) => {
                screen.set_property_from_str("capture-api", "wgc");
                screen.set_property("window-handle", handle);
            }
        }
    }
}

/// The caps that make a Windows screen capture deliver `fps` frames per
/// second, and leave them on the graphics card.
#[cfg(windows)]
fn capture_caps(fps: u32) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .features([SIZED_MEMORY])
        .field("framerate", gst::Fraction::new(fps.max(1) as i32, 1))
        .build()
}

/// A share as the room sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sharing {
    /// Size of the picture being sent.
    pub width: u32,
    pub height: u32,
    /// Size of the screen it is taken from, which is larger if the sharer
    /// has it scaled down.
    pub source_width: u32,
    pub source_height: u32,
    /// Frames per second the sharer asked for.
    pub fps: u32,
    pub codec: Codec,
    /// Bitrate the encoder aims for.
    pub kbps: u32,
    /// Frames per second actually sent of late.
    pub sent_fps: f64,
    /// Time from a frame being captured to it being encoded.
    pub encode_ms: f64,
    /// Whether sound comes with the picture.
    pub sound: bool,
    /// Who the stream is being sent to.
    pub watchers: Vec<PeerId>,
}

impl Sharing {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
            "codec": self.codec.id(),
            "kbps": self.kbps,
            "sentFps": self.sent_fps,
            "sourceWidth": self.source_width,
            "sourceHeight": self.source_height,
            "encodeMs": self.encode_ms,
            "sound": self.sound,
            "watchers": self.watchers,
        })
    }

    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        let number = |field: &str| value[field].as_u64().and_then(|n| u32::try_from(n).ok());
        Some(Self {
            width: number("width")?,
            height: number("height")?,
            fps: number("fps")?,
            // Clients from before there was a choice sent H.264.
            codec: match value["codec"].as_str() {
                Some(id) => Codec::from_id(id)?,
                None => Codec::H264,
            },
            kbps: number("kbps")?,
            sent_fps: value["sentFps"].as_f64().unwrap_or(0.0),
            source_width: number("sourceWidth").or(number("width"))?,
            source_height: number("sourceHeight").or(number("height"))?,
            encode_ms: value["encodeMs"].as_f64().unwrap_or(0.0),
            sound: value["sound"].as_bool().unwrap_or(false),
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
pub fn fit(source: (u32, u32), max_height: Option<u32>, codec: Codec) -> (u32, u32) {
    let (width, height) = (source.0.max(2), source.1.max(2));
    let mut scale = 1.0_f64;
    if let Some(max) = max_height {
        scale = scale.min(f64::from(max) / f64::from(height));
    }
    let max_side = f64::from(codec.max_side());
    scale = scale
        .min(max_side / f64::from(width))
        .min(max_side / f64::from(height));
    // The encoder works on even sizes only.
    if scale >= 1.0 {
        return (width & !1, height & !1);
    }
    let even = |side: u32| (((f64::from(side) * scale / 2.0).round() as u32) * 2).max(2);
    (even(width), even(height))
}

/// The bitrate for a picture of this size and rate, when the sharer has not
/// set one. Half the frames do not halve the bits: with more time between
/// frames, more changes from one to the next.
pub fn bitrate_kbps(width: u32, height: u32, fps: u32, codec: Codec) -> u32 {
    let fps = f64::from(fps.max(1));
    let bits_per_pixel = BITS_PER_PIXEL_AT_60 * (60.0 / fps).sqrt() * codec.bits_needed();
    let kbps = f64::from(width) * f64::from(height) * fps * bits_per_pixel / 1000.0;
    ((kbps / 100.0).round() as u32 * 100).max(MIN_KBPS)
}

// --- Sending ----------------------------------------------------------------

/// What the encoder reports from GStreamer's threads.
pub(crate) enum EncoderEvent {
    /// An encoded frame, with the time it was captured as running time of
    /// the call's pipeline.
    Frame(Codec, gst::Buffer),
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
    /// Where the encoded frames leave the pipeline.
    out: gst::Element,
    /// Tells the encoder how many frames per second to expect.
    pace: gst::Element,
    state: Arc<Mutex<EncoderState>>,
    counters: Arc<EncoderCounters>,
    /// When the counters were last read.
    last_read: Mutex<Instant>,
    events: Arc<dyn Fn(EncoderEvent) + Send + Sync>,
    /// What is being shared. On Linux this holds the connection the screen
    /// is read through, which has to stay open for as long.
    _source: ShareSource,
    #[cfg(windows)]
    _timers: FineTimers,
}

struct EncoderState {
    settings: ShareSettings,
    /// Size of what the source delivers, once known.
    source: Option<(u32, u32)>,
    /// Size and bitrate being sent.
    format: Option<(u32, u32, u32)>,
    /// The part of the pipeline that belongs to the codec in use.
    stage: EncodingStage,
}

/// The elements that encode with one codec, between the sized frames and
/// the pipeline's exit.
#[derive(Clone)]
struct EncodingStage {
    bin: gst::Bin,
    encoder: gst::Element,
}

impl EncodingStage {
    fn new(codec: Codec) -> Result<Self> {
        let description = codec.encoder_description();
        debug!(%description, "building the encoding stage");
        let bin = gst::parse::bin_from_description(&description, true)
            .with_context(|| format!("cannot encode {} on this machine", codec.label()))?;
        let encoder = bin
            .by_name("encoder")
            .ok_or_else(|| anyhow!("the encoding stage has no encoder"))?;
        Ok(Self { bin, encoder })
    }

    /// Puts the stage in place between `size` and `out`.
    fn place(
        &self,
        pipeline: &gst::Pipeline,
        size: &gst::Element,
        out: &gst::Element,
    ) -> Result<()> {
        pipeline.add(&self.bin)?;
        size.link(&self.bin)?;
        self.bin.link(out)?;
        Ok(())
    }
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
            ShareSource::Screen(screen) => screen.description(settings.fps),
        };
        // A screen delivers frames at no fixed rate, and says so. The
        // encoder shares out its bitrate by the rate it expects, so it is
        // told the most it can get.
        let description = format!(
            "{source_description} \
             ! videorate name=rate drop-only=true max-rate={} \
             ! capssetter name=pace join=true replace=false caps=\"{}\" \
             ! {SIZING} \
             ! capsfilter name=size \
             appsink name=out sync=false",
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
        let (rate, size, out) = (by_name("rate")?, by_name("size")?, by_name("out")?);
        let pace = by_name("pace")?;
        let stage = EncodingStage::new(settings.codec)?;
        stage.place(&pipeline, &size, &out)?;
        if let ShareSource::Screen(screen) = source {
            screen.apply(&by_name("screen")?);
        }
        let state = Arc::new(Mutex::new(EncoderState {
            settings,
            source: None,
            format: None,
            stage,
        }));
        let counters = Arc::new(EncoderCounters::default());
        let events: Arc<dyn Fn(EncoderEvent) + Send + Sync> = Arc::new(events);

        // The source says how large its picture is, also when that changes
        // later. Everything after it follows.
        let weak_size = size.downgrade();
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
                if let Some(size) = weak_size.upgrade() {
                    let mut state = probe_state.lock().unwrap();
                    state.source = Some((video.width(), video.height()));
                    state.configure(&size);
                    drop(state);
                    probe_events(EncoderEvent::Format);
                }
                gst::PadProbeReturn::Ok
            });

        // Both pipelines count time on the same clock, but from different
        // starting points.
        pipeline.use_clock(Some(clock));
        let sink = out
            .clone()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("the encoder's output is not an appsink"))?;
        let (sink_counters, sink_events) = (counters.clone(), events.clone());
        let (sink_clock, weak_pipeline) = (clock.clone(), pipeline.downgrade());
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let mut buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    // The frame itself says which codec made it: around a
                    // change of codec, the settings may be a frame ahead.
                    let codec = sample
                        .caps()
                        .and_then(Codec::of_caps)
                        .ok_or(gst::FlowError::NotNegotiated)?;
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
                    sink_events(EncoderEvent::Frame(codec, buffer));
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let bus = pipeline.bus().expect("pipelines always have a bus");
        let bus_events = events.clone();
        bus.set_sync_handler(move |_, message| {
            let events = &bus_events;
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
            out,
            pace,
            state,
            counters,
            last_read: Mutex::new(Instant::now()),
            events,
            _source: source.clone(),
            #[cfg(windows)]
            _timers: FineTimers::new(),
        };
        this.pipeline
            .set_state(gst::State::Playing)
            .context("cannot start the video encoder")?;
        Ok(this)
    }

    /// Changes the size, rate, bitrate or codec of what is sent, without a
    /// break. A codec this machine cannot encode leaves the codec as it is.
    pub fn set_settings(&self, mut settings: ShareSettings) {
        let mut state = self.state.lock().unwrap();
        if state.settings == settings {
            return;
        }
        let new_stage = if state.settings.codec == settings.codec {
            None
        } else {
            EncodingStage::new(settings.codec)
                .inspect_err(|err| {
                    warn!("staying with {}: {err:#}", state.settings.codec.label());
                    settings.codec = state.settings.codec;
                })
                .ok()
        };
        state.settings = settings;
        self.rate.set_property("max-rate", settings.fps as i32);
        #[cfg(windows)]
        if let Some(capture_rate) = self.pipeline.by_name("capture_rate") {
            capture_rate.set_property("caps", capture_caps(settings.fps));
        }
        self.pace.set_property("caps", pace_caps(settings.fps));
        // Makes the new pace take effect with the next frame, whether or
        // not anything else about the picture changes.
        if let Some(pad) = self.pace.static_pad("src") {
            pad.send_event(gst::event::Reconfigure::new());
        }
        match new_stage {
            // The new encoder is set up once it is in place.
            Some(stage) => {
                drop(state);
                self.replace_stage(stage);
            }
            None => state.configure(&self.size),
        }
    }

    /// Swaps the encoding stage for `new` between two frames. The first
    /// frame of the new one is a keyframe, so viewers carry straight on.
    fn replace_stage(&self, new: EncodingStage) {
        let Some(pad) = self.size.static_pad("src") else {
            return;
        };
        let pending = Mutex::new(Some(new));
        let (pipeline, size, out) = (
            self.pipeline.downgrade(),
            self.size.downgrade(),
            self.out.downgrade(),
        );
        let (state, events) = (self.state.clone(), self.events.clone());
        // Runs when no frame is on its way through the pad: at once, on
        // this thread, or after the frame that is.
        pad.add_probe(gst::PadProbeType::IDLE, move |_, _| {
            let (Some(new), Some(pipeline), Some(size), Some(out)) = (
                pending.lock().unwrap().take(),
                pipeline.upgrade(),
                size.upgrade(),
                out.upgrade(),
            ) else {
                return gst::PadProbeReturn::Remove;
            };
            let old = std::mem::replace(&mut state.lock().unwrap().stage, new.clone());
            size.unlink(&old.bin);
            old.bin.unlink(&out);
            let _ = pipeline.remove(&old.bin);
            // Whatever the old encoder still held is not sent.
            let _ = old.bin.set_state(gst::State::Null);
            let placed = new
                .place(&pipeline, &size, &out)
                .and_then(|()| Ok(new.bin.sync_state_with_parent()?));
            match placed {
                Ok(()) => {
                    state.lock().unwrap().configure(&size);
                    events(EncoderEvent::Format);
                }
                Err(err) => events(EncoderEvent::Ended(Some(format!(
                    "cannot change the codec of the shared screen: {err:#}"
                )))),
            }
            gst::PadProbeReturn::Remove
        });
    }

    /// The size of the picture being sent and the bitrate aimed for, once
    /// the source has said what it delivers.
    pub fn format(&self) -> Option<(u32, u32, u32)> {
        self.state.lock().unwrap().format
    }

    /// The size of what the source delivers, once it has said.
    pub fn source(&self) -> Option<(u32, u32)> {
        self.state.lock().unwrap().source
    }

    pub fn settings(&self) -> ShareSettings {
        self.state.lock().unwrap().settings
    }

    /// Makes the next frame one that can be decoded on its own.
    pub fn force_keyframe(&self) {
        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        let encoder = self.state.lock().unwrap().stage.encoder.clone();
        if let Some(pad) = encoder.static_pad("src") {
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
    fn configure(&mut self, size: &gst::Element) {
        let Some(source) = self.source else {
            return;
        };
        let ShareSettings {
            max_height,
            fps,
            codec,
            kbps,
            ..
        } = self.settings;
        let (width, height) = fit(source, max_height, codec);
        let kbps = match kbps {
            Some(kbps) => kbps.clamp(MIN_KBPS, MAX_KBPS),
            None => bitrate_kbps(width, height, fps, codec),
        };
        let encoder = &self.stage.encoder;
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", width as i32)
            .field("height", height as i32)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
        #[cfg(windows)]
        let caps = caps.features([SIZED_MEMORY]);
        size.set_property("caps", caps.build());
        encoder.set_property("bitrate", kbps);
        let burst = f64::from(kbps) * FRAMES_OF_BURST / f64::from(fps.max(1));
        encoder.set_property("vbv-buffer-size", burst.round() as u32);
        debug!(?source, width, height, kbps, ?codec, "encoder configured");
        self.format = Some((width, height, kbps));
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // The screen goes first, so that its thread is gone by the time
        // the other elements hand back the frames they still hold. See
        // `Codec::encoder_description` for what happens otherwise.
        if let Some(screen) = self.pipeline.by_name("screen") {
            let _ = screen.set_state(gst::State::Null);
        }
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
    from: PeerId,
    codec: Codec,
    frames: FrameSink,
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
            caps.features([GPU_MEMORY]).build()
        } else {
            caps.build()
        };
        size.set_property("caps", caps);
    }
}

/// The elements that take frames to the graphics card and convert and scale
/// them there, the kind of memory they are in while there, and the element
/// that brings them back.
#[cfg(not(windows))]
const GPU_PREPARE: &str = "glupload ! glcolorconvert ! glcolorscale";
#[cfg(not(windows))]
const GPU_MEMORY: &str = "memory:GLMemory";
#[cfg(not(windows))]
const GPU_DOWNLOAD: &str = "gldownload";
// Windows has the OpenGL elements too, and they pass the test below. But a
// change of size in the middle of a stream stops them (GStreamer 1.28.9).
#[cfg(windows)]
const GPU_PREPARE: &str = "d3d11upload ! d3d11convert";
#[cfg(windows)]
const GPU_MEMORY: &str = "memory:D3D11Memory";
#[cfg(windows)]
const GPU_DOWNLOAD: &str = "d3d11download";

/// Whether frames can be converted and scaled on the graphics card on this
/// system. Tried once, with a single small frame.
fn gpu_conversion_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        let attempt = || -> Result<bool> {
            let pipeline = gst::parse::launch(&format!(
                "videotestsrc num-buffers=1 ! video/x-raw,format=NV12,width=64,height=64 \
                 ! {GPU_PREPARE} \
                 ! video/x-raw({GPU_MEMORY}),format=RGBA,width=32,height=32 \
                 ! {GPU_DOWNLOAD} ! video/x-raw,format=RGBA ! fakesink"
            ))?;
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
    pub fn start(from: PeerId, frames: FrameSink, max_height: u32, codec: Codec) -> Result<Self> {
        // Turning what the decoder puts out into pixels for display, at the
        // size they are shown at, is quick on the graphics card and slow
        // on the processor.
        let on_gpu = gpu_conversion_works();
        let prepare = if on_gpu {
            format!(
                "{GPU_PREPARE} ! capsfilter name=size ! {GPU_DOWNLOAD} ! video/x-raw,format=RGBA"
            )
        } else {
            "videoconvertscale n-threads=4 ! capsfilter name=size".to_owned()
        };
        // Encoded frames cannot be skipped, because the ones after them
        // build on them. Decoded ones can, so when preparing frames for
        // display falls behind, frames are dropped ahead of that step.
        let description = format!(
            "appsrc name=in is-live=true format=time caps={} \
             ! {} \
             ! queue leaky=downstream max-size-buffers=2 max-size-time=0 max-size-bytes=0 \
             ! {prepare} \
             ! appsink name=out sync=false max-buffers=2 drop=true",
            codec.caps(),
            codec.decoder_description()
        );
        let pipeline = gst::parse::launch(&description)
            .with_context(|| format!("cannot decode {} on this machine", codec.label()))?
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
        let sink_frames = frames.clone();
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
                            sink_frames(
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
            from,
            codec,
            frames,
            pipeline,
            input,
            size,
            on_gpu,
            state,
            counters,
            last_read: Mutex::new(Instant::now()),
        })
    }

    /// The codec of the stream it decodes.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// A decoder like this one for a stream in another codec.
    pub fn for_codec(&self, codec: Codec) -> Result<Self> {
        let max_height = self.state.lock().unwrap().max_height;
        Self::start(self.from, self.frames.clone(), max_height, codec)
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
        assert_eq!(fit((2560, 1440), Some(1080), Codec::H264), (1920, 1080));
        assert_eq!(fit((2560, 1440), Some(2160), Codec::H264), (2560, 1440));
        assert_eq!(fit((2560, 1440), None, Codec::H264), (2560, 1440));
        assert_eq!(fit((3440, 1440), Some(720), Codec::Hevc), (1720, 720));
    }

    #[test]
    fn sizes_stay_within_what_the_encoder_takes() {
        assert_eq!(fit((5120, 1440), None, Codec::H264), (4096, 1152));
        assert_eq!(fit((5120, 1440), None, Codec::Hevc), (5120, 1440));
        assert_eq!(fit((1281, 721), None, Codec::H264), (1280, 720));
        let (width, height) = fit((1366, 768), Some(720), Codec::H264);
        assert_eq!((width % 2, height), (0, 720));
    }

    #[test]
    fn sizes_are_read_as_written() {
        assert_eq!(parse_size("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_size("1080p"), None);
    }

    #[test]
    fn bitrate_follows_size_and_rate() {
        assert_eq!(bitrate_kbps(1920, 1080, 60, Codec::H264), 8700);
        assert_eq!(bitrate_kbps(3840, 2160, 60, Codec::H264), 34800);
        assert_eq!(bitrate_kbps(1920, 1080, 30, Codec::H264), 6200);
        assert_eq!(bitrate_kbps(1920, 1080, 60, Codec::Hevc), 6500);
    }

    #[test]
    fn codecs_go_by_the_names_they_are_given() {
        gst::init().unwrap();
        for codec in Codec::ALL {
            assert_eq!(Codec::from_id(codec.id()), Some(codec));
            assert_eq!(Codec::from_rtp_name(codec.rtp_name()), Some(codec));
            let caps = codec.caps().parse::<gst::Caps>().unwrap();
            assert_eq!(Codec::of_caps(&caps), Some(codec));
        }
        assert_eq!(Codec::from_id("av1"), None);
    }

    #[test]
    fn a_share_survives_the_trip_as_json() {
        let sharing = Sharing {
            width: 1920,
            height: 1080,
            source_width: 2560,
            source_height: 1440,
            fps: 60,
            codec: Codec::Hevc,
            kbps: 8700,
            sent_fps: 59.5,
            encode_ms: 4.25,
            sound: true,
            watchers: vec![2, 5],
        };
        assert_eq!(Sharing::from_json(&sharing.to_json()), Some(sharing));
        assert_eq!(Sharing::from_json(&Value::Null), None);
    }
}
