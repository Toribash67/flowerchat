//! The sound that goes with a shared screen: everything this machine plays,
//! except what we play ourselves.
//!
//! How that is collected differs by system: see `pipewire` for Linux and
//! `loopback` for Windows.

#[cfg(windows)]
#[path = "sound/loopback.rs"]
mod programs;
#[cfg(not(windows))]
#[path = "sound/pipewire.rs"]
mod programs;

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use tracing::warn;

/// The format the sound has between capture and encoder, and between
/// decoder and playback.
pub(crate) const STEREO: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";
/// The Opus encoder for it: music and effects, not just speech.
const ENCODER: &str = "opusenc audio-type=generic bitrate=128000 frame-size=10";

/// Where the sound comes from.
pub(crate) enum SoundSource {
    /// What the other programs on this machine play.
    Programs,
    /// A steady tone, to go with a test picture.
    Tone,
}

/// Sound on its way to the viewers, for as long as this lives.
///
/// It is captured and encoded in a pipeline of its own. The recording
/// stream delivers nothing while no program plays, and a source that has
/// not delivered anything yet holds up the pipeline it is in: inside the
/// call's pipeline it would stop the call until something made a sound.
pub(crate) struct StreamSound {
    pipeline: gst::Pipeline,
    _collector: Option<programs::Collector>,
}

impl StreamSound {
    /// Starts capturing. `packets` gets the sound as Opus packets, with the
    /// time each was captured as running time of the pipeline they are
    /// headed for, whose `clock` and `base_time` these are.
    pub fn start(
        source: SoundSource,
        clock: &gst::Clock,
        base_time: gst::ClockTime,
        packets: impl Fn(gst::Sample) + Send + Sync + 'static,
    ) -> Result<Self> {
        let collecting = matches!(source, SoundSource::Programs);
        let description = match source {
            SoundSource::Tone => {
                "audiotestsrc is-live=true wave=sine freq=440 volume=0.1 samplesperbuffer=480"
                    .to_owned()
            }
            SoundSource::Programs => programs::description(),
        };
        let pipeline = gst::parse::launch(&format!(
            "{description} ! {STEREO} ! audioconvert ! audioresample ! {STEREO} ! {ENCODER} \
             ! appsink name=out sync=false async=false"
        ))
        .context("cannot build the capture of the stream's sound")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("the sound capture is not a pipeline"))?;
        let collector = if collecting {
            let capture = pipeline
                .by_name("capture")
                .ok_or_else(|| anyhow!("the sound capture has no source"))?;
            Some(programs::collect(&capture)?)
        } else {
            None
        };

        // Both pipelines count time on the same clock, but from different
        // starting points.
        pipeline.use_clock(Some(clock));
        let (clock, weak_pipeline) = (clock.clone(), pipeline.downgrade());
        pipeline
            .by_name("out")
            .and_then(|out| out.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("the sound capture has no exit"))?
            .set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let own_base = weak_pipeline.upgrade().and_then(|p| p.base_time());
                        let captured = sample
                            .segment()
                            .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
                            .zip(sample.buffer().and_then(|buffer| buffer.pts()))
                            .and_then(|(segment, pts)| segment.to_running_time(pts));
                        let (Some(own_base), Some(captured), Some(mut buffer)) =
                            (own_base, captured, sample.buffer_owned())
                        else {
                            return Ok(gst::FlowSuccess::Ok);
                        };
                        // A packet cannot have been captured later than now.
                        let captured_at = (own_base + captured).min(clock.time());
                        let buffer_mut = buffer.make_mut();
                        buffer_mut.set_pts(captured_at.saturating_sub(base_time));
                        buffer_mut.set_dts(gst::ClockTime::NONE);
                        let caps = sample.caps_owned();
                        let mut packet = gst::Sample::builder().buffer(&buffer);
                        if let Some(caps) = &caps {
                            packet = packet.caps(caps);
                        }
                        packets(packet.build());
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );

        let bus = pipeline.bus().expect("pipelines always have a bus");
        bus.set_sync_handler(|_, message| {
            if let gst::MessageView::Error(err) = message.view() {
                warn!(
                    "the stream's sound has stopped: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
            gst::BusSyncReply::Drop
        });
        // The recording stream only counts as started once something is
        // connected to it, and starting it waits for that: until a program
        // plays, which can be never. So nobody waits for the start.
        pipeline.call_async(|pipeline| {
            let _ = pipeline.set_state(gst::State::Playing);
        });
        Ok(Self {
            pipeline,
            _collector: collector,
        })
    }
}

impl Drop for StreamSound {
    fn drop(&mut self) {
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        // Stopping has to wait for a start that is still under way, which
        // is told to give up first. Nobody waits for the stop either.
        self.pipeline.call_async(|pipeline| {
            if let Some(capture) = pipeline.by_name("capture") {
                capture.send_event(gst::event::FlushStart::new());
            }
            let _ = pipeline.set_state(gst::State::Null);
        });
    }
}
