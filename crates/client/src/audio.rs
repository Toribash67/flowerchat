//! Audio settings and devices: what can be chosen, what is plugged in, and
//! the two things a settings screen wants outside a call: a microphone level
//! and a test sound.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use gstreamer as gst;
use tracing::warn;

/// The format audio has between capture and encoder, and between decoders
/// and playback. `webrtcdsp` needs exactly this layout.
pub(crate) const RAW_AUDIO: &str =
    "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1";
/// How often loudness is reported, for level meters and speaking indicators.
pub(crate) const LEVEL_INTERVAL_NS: u64 = 50_000_000;
/// Largest receive buffer a call accepts.
pub const MAX_JITTER_MS: u32 = 500;

const TEST_SOUND: Duration = Duration::from_millis(600);

/// Everything about a call's audio that can change while it is under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    /// Microphone, as an [`AudioDevice::id`], or the system default.
    pub input: Option<String>,
    /// Speakers, as an [`AudioDevice::id`], or the system default.
    pub output: Option<String>,
    /// Remove what the speakers play from what the microphone picks up.
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    /// Even out how loud the microphone is.
    pub automatic_volume: bool,
    /// Size of the receive buffer that absorbs network jitter.
    pub jitter_ms: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            echo_cancellation: true,
            noise_suppression: true,
            automatic_volume: true,
            jitter_ms: 40,
        }
    }
}

impl AudioSettings {
    /// The same settings with the microphone sent exactly as captured.
    pub fn unprocessed(mut self) -> Self {
        self.echo_cancellation = false;
        self.noise_suppression = false;
        self.automatic_volume = false;
        self
    }

    /// Whether changing from `self` to `other` needs a new capture chain.
    pub(crate) fn captures_like(&self, other: &Self) -> bool {
        self.input == other.input
            && self.echo_cancellation == other.echo_cancellation
            && self.noise_suppression == other.noise_suppression
            && self.automatic_volume == other.automatic_volume
    }
}

/// Builds the chain from the microphone to processed audio in [`RAW_AUDIO`],
/// as a bin with one source pad.
///
/// `echo_probe` names the `webrtcechoprobe` that hears what the speakers
/// play. Without one there is nothing to cancel echo against, and echo
/// cancellation stays off whatever the settings say.
pub(crate) fn capture_bin(settings: &AudioSettings, echo_probe: Option<&str>) -> Result<gst::Bin> {
    let echo = settings.echo_cancellation && echo_probe.is_some();
    let processing = if echo || settings.noise_suppression || settings.automatic_volume {
        let probe = echo_probe.map_or_else(String::new, |name| format!("probe={name}"));
        format!(
            "! webrtcdsp {probe} echo-cancel={echo} noise-suppression={} gain-control={} \
             high-pass-filter=true",
            settings.noise_suppression, settings.automatic_volume
        )
    } else {
        String::new()
    };
    // The microphone never supplies the pipeline's clock, so that it can be
    // replaced without the pipeline losing its clock.
    let bin = gst::parse::bin_from_description(
        &format!(
            "pulsesrc name=mic provide-clock=false buffer-time=20000 latency-time=10000 \
             ! audioconvert ! audioresample ! capsfilter caps={RAW_AUDIO} {processing}"
        ),
        true,
    )
    .context("cannot build the microphone chain")?;
    if let Some(device) = &settings.input {
        bin.by_name("mic")
            .ok_or_else(|| anyhow!("the microphone chain has no microphone"))?
            .set_property("device", device);
    }
    Ok(bin)
}

/// A name no other echo probe in this process has.
pub(crate) fn unique_probe_name() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("echo-probe-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The loudness in a message from a `level` element, in dB below full scale.
pub(crate) fn level_db(message: &gst::Message) -> Option<f32> {
    let gst::MessageView::Element(element) = message.view() else {
        return None;
    };
    let level = element.structure().filter(|s| s.name() == "level")?;
    let rms = level.get::<&gst::glib::ValueArray>("rms").ok()?;
    let db = rms.first()?.get::<f64>().ok()?;
    Some(db as f32)
}

// --- Devices ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// What [`AudioSettings`] refers to the device by.
    pub id: String,
    /// The device's name as the system shows it.
    pub label: String,
    /// Whether this is what "system default" currently means.
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDevices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

/// Keeps track of the audio devices that are plugged in.
pub struct DeviceWatch {
    monitor: gst::DeviceMonitor,
}

impl DeviceWatch {
    pub fn start() -> Result<Self> {
        let monitor = gst::DeviceMonitor::new();
        for class in ["Audio/Source", "Audio/Sink"] {
            monitor
                .add_filter(Some(class), None)
                .ok_or_else(|| anyhow!("nothing on this system lists {class} devices"))?;
        }
        monitor.start().context("cannot watch audio devices")?;
        Ok(Self { monitor })
    }

    /// The devices present right now.
    pub fn devices(&self) -> AudioDevices {
        let mut devices = AudioDevices::default();
        for device in self.monitor.devices() {
            let Some(properties) = device.properties() else {
                continue;
            };
            // PipeWire's name for the device is also the name its PulseAudio
            // side, which the pipelines go through, knows it by.
            let Ok(id) = properties.get::<String>("node.name") else {
                continue;
            };
            let entry = AudioDevice {
                id,
                label: device.display_name().into(),
                is_default: properties.get::<bool>("is-default").unwrap_or(false),
            };
            let list = if device.has_classes("Audio/Source") {
                &mut devices.inputs
            } else {
                &mut devices.outputs
            };
            if !list.contains(&entry) {
                list.push(entry);
            }
        }
        for list in [&mut devices.inputs, &mut devices.outputs] {
            list.sort_by(|a, b| a.label.cmp(&b.label));
        }
        devices
    }

    /// Whether anything was plugged in, unplugged or changed since the last
    /// call.
    pub fn changed(&self) -> bool {
        let bus = self.monitor.bus();
        let mut changed = false;
        while bus.pop().is_some() {
            changed = true;
        }
        changed
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        self.monitor.stop();
    }
}

// --- Outside a call ---------------------------------------------------------

/// Listens to the microphone and reports how loud it is, without sending
/// the sound anywhere. Stops when dropped.
pub struct MicCheck {
    pipeline: gst::Pipeline,
}

impl MicCheck {
    /// `on_level` gets the loudness in dB below full scale about twenty
    /// times a second, on one of GStreamer's threads.
    pub fn start(
        settings: &AudioSettings,
        on_level: impl Fn(f32) + Send + Sync + 'static,
    ) -> Result<Self> {
        let pipeline = gst::Pipeline::new();
        let capture = capture_bin(settings, None)?;
        let meter = gst::parse::bin_from_description(
            &format!("level interval={LEVEL_INTERVAL_NS} post-messages=true ! fakesink"),
            true,
        )?;
        pipeline.add_many([&capture, &meter])?;
        capture.link(&meter)?;

        let bus = pipeline.bus().expect("pipelines always have a bus");
        bus.set_sync_handler(move |_, message| {
            if let Some(db) = level_db(message) {
                on_level(db);
            } else if let gst::MessageView::Error(err) = message.view() {
                warn!("cannot listen to the microphone: {}", err.error());
            }
            gst::BusSyncReply::Drop
        });
        pipeline
            .set_state(gst::State::Playing)
            .context("cannot open the microphone")?;
        Ok(Self { pipeline })
    }
}

impl Drop for MicCheck {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
    }
}

/// Plays a short tone on `output`, or on the default speakers. Returns as
/// soon as the tone has started.
pub fn play_test_sound(output: Option<&str>) -> Result<()> {
    let buffers = TEST_SOUND.as_millis() / 10;
    let pipeline = gst::parse::launch(&format!(
        "audiotestsrc wave=sine freq=523.25 volume=0.2 samplesperbuffer=480 \
           num-buffers={buffers} \
         ! {RAW_AUDIO} ! audioconvert ! audioresample ! pulsesink name=speakers"
    ))
    .context("cannot build the test sound")?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("the test sound is not a pipeline"))?;
    if let Some(device) = output {
        pipeline
            .by_name("speakers")
            .ok_or_else(|| anyhow!("the test sound has no speakers"))?
            .set_property("device", device);
    }
    pipeline
        .set_state(gst::State::Playing)
        .context("cannot open the speakers")?;

    // Someone has to wait for the end and close the device again.
    std::thread::spawn(move || {
        let bus = pipeline.bus().expect("pipelines always have a bus");
        let ended = [gst::MessageType::Eos, gst::MessageType::Error];
        let wait = gst::ClockTime::from_mseconds(TEST_SOUND.as_millis() as u64 + 5000);
        if let Some(message) = bus.timed_pop_filtered(wait, &ended)
            && let gst::MessageView::Error(err) = message.view()
        {
            warn!("cannot play the test sound: {}", err.error());
        }
        let _ = pipeline.set_state(gst::State::Null);
    });
    Ok(())
}
