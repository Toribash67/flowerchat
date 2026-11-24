//! The sound that goes with a shared screen: everything this machine plays,
//! except what we play ourselves.
//!
//! PipeWire lets a program's sound go to more than one place. We open a
//! recording stream that is connected to nothing, and connect every other
//! program's playback to it, next to wherever that already goes. Our own
//! playback is left out, so the people in the call do not get their own
//! voices back.
//!
//! PipeWire's own tools do the watching and connecting: `pw-dump` reports
//! what plays, as it comes and goes, and `pw-link` makes the connections.
//! They disappear by themselves when the recording stream closes.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use serde_json::Value;
use tracing::{debug, warn};

/// The format the sound has between capture and encoder, and between
/// decoder and playback.
pub(crate) const STEREO: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";
/// The Opus encoder for it: music and effects, not just speech.
const ENCODER: &str = "opusenc audio-type=generic bitrate=128000 frame-size=10";
/// Programs whose playback is never collected: other copies of ourselves on
/// this machine, which play the same call.
const OUR_BINARIES: [&str; 2] = ["flowerchat", "flowerchat-cli"];

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
    _collector: Option<Collector>,
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
        let (description, node) = match source {
            SoundSource::Tone => (
                "audiotestsrc is-live=true wave=sine freq=440 volume=0.1 samplesperbuffer=480"
                    .to_owned(),
                None,
            ),
            SoundSource::Programs => (
                "pipewiresrc name=capture autoconnect=false provide-clock=false \
                 do-timestamp=true client-name=flowerchat"
                    .to_owned(),
                Some(unique_node_name()),
            ),
        };
        let pipeline = gst::parse::launch(&format!(
            "{description} ! {STEREO} ! audioconvert ! audioresample ! {STEREO} ! {ENCODER} \
             ! appsink name=out sync=false async=false"
        ))
        .context("cannot build the capture of the stream's sound")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("the sound capture is not a pipeline"))?;
        let collector = match &node {
            Some(node) => {
                let properties = gst::Structure::builder("props")
                    .field("node.name", node.as_str())
                    .field("node.description", "flowerchat stream sound")
                    .field("media.class", "Stream/Input/Audio")
                    .build();
                pipeline
                    .by_name("capture")
                    .ok_or_else(|| anyhow!("the sound capture has no source"))?
                    .set_property("stream-properties", properties);
                Some(Collector::start(node)?)
            }
            None => None,
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

/// A name no other recording stream on this machine has.
fn unique_node_name() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "flowerchat-stream-sound-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Keeps every other program's playback connected to our recording stream.
struct Collector {
    dump: Child,
    worker: Option<JoinHandle<()>>,
}

impl Collector {
    /// `node` is the name of the recording stream to collect into. It need
    /// not exist yet.
    fn start(node: &str) -> Result<Self> {
        let mut dump = Command::new("pw-dump")
            .args(["--monitor", "--no-colors"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot run pw-dump, which comes with PipeWire")?;
        let reports = dump.stdout.take().expect("stdout was asked for");
        let node = node.to_owned();
        let worker = std::thread::Builder::new()
            .name("stream-sound".into())
            .spawn(move || follow(reports, &node))
            .context("cannot start following what plays")?;
        Ok(Self {
            dump,
            worker: Some(worker),
        })
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        // The reports end with the process, and the worker with them.
        let _ = self.dump.kill();
        let _ = self.dump.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Reads `pw-dump`'s reports until they end, and connects what they show to
/// be missing.
fn follow(reports: ChildStdout, node: &str) {
    let mut graph = Graph::new(node, std::process::id());
    // Each report is a JSON array of the objects that are new or changed,
    // one array after the other.
    let reports = serde_json::Deserializer::from_reader(BufReader::new(reports));
    for report in reports.into_iter::<Value>() {
        let Ok(report) = report else {
            break;
        };
        for object in report.as_array().into_iter().flatten() {
            graph.update(object);
        }
        for (output, input) in graph.missing() {
            let made = Command::new("pw-link")
                .args([output.to_string(), input.to_string()])
                .stdin(Stdio::null())
                .output();
            match made {
                Ok(done) if done.status.success() => debug!(output, input, "collecting"),
                // Mostly a connection that exists already.
                Ok(done) => debug!(
                    output,
                    input,
                    "not connected: {}",
                    String::from_utf8_lossy(&done.stderr).trim()
                ),
                Err(err) => {
                    warn!("cannot run pw-link, which comes with PipeWire: {err}");
                    return;
                }
            }
            graph.connected.insert((output, input));
        }
    }
}

/// What PipeWire has told us of itself, as far as it matters here.
struct Graph {
    /// Name of our recording stream.
    target: String,
    our_pid: u64,
    nodes: HashMap<u64, Node>,
    ports: HashMap<u64, Port>,
    /// Process id and program of each client connected to PipeWire.
    clients: HashMap<u64, (Option<u64>, Option<String>)>,
    /// The connections we have asked for.
    connected: HashSet<(u64, u64)>,
}

struct Node {
    name: String,
    /// Whether this is a program playing sound.
    playback: bool,
    client: Option<u64>,
    pid: Option<u64>,
    binary: Option<String>,
}

struct Port {
    node: u64,
    output: bool,
    /// Which channel it carries: `FL`, `FR`, `MONO`, ...
    channel: String,
}

impl Graph {
    fn new(target: &str, our_pid: u32) -> Self {
        Self {
            target: target.to_owned(),
            our_pid: u64::from(our_pid),
            nodes: HashMap::new(),
            ports: HashMap::new(),
            clients: HashMap::new(),
            connected: HashSet::new(),
        }
    }

    /// Takes in one object of a report: something new, changed, or gone.
    fn update(&mut self, object: &Value) {
        let Some(id) = object["id"].as_u64() else {
            return;
        };
        let info = &object["info"];
        if info.is_null() {
            self.nodes.remove(&id);
            self.clients.remove(&id);
            if self.ports.remove(&id).is_some() {
                self.connected
                    .retain(|(output, input)| *output != id && *input != id);
            }
            return;
        }
        let props = &info["props"];
        let text = |key: &str| props[key].as_str().map(str::to_owned);
        // Numbers come as numbers from some clients and as text from others.
        let number = |key: &str| {
            props[key]
                .as_u64()
                .or_else(|| props[key].as_str()?.parse().ok())
        };
        let pid = || number("application.process.id").or_else(|| number("pipewire.sec.pid"));
        match object["type"].as_str().unwrap_or_default() {
            "PipeWire:Interface:Node" => {
                let node = Node {
                    name: text("node.name").unwrap_or_default(),
                    playback: props["media.class"] == "Stream/Output/Audio",
                    client: number("client.id"),
                    pid: pid(),
                    binary: text("application.process.binary"),
                };
                self.nodes.insert(id, node);
            }
            "PipeWire:Interface:Client" => {
                self.clients
                    .insert(id, (pid(), text("application.process.binary")));
            }
            "PipeWire:Interface:Port" => {
                let (Some(node), Some(direction)) = (number("node.id"), info["direction"].as_str())
                else {
                    return;
                };
                // A playback stream's monitor ports repeat its sound.
                if props["port.monitor"] == true {
                    return;
                }
                let port = Port {
                    node,
                    output: direction == "output",
                    channel: text("audio.channel").unwrap_or_default(),
                };
                self.ports.insert(id, port);
            }
            _ => {}
        }
    }

    /// Whether a node's sound is our own, which is not collected.
    fn ours(&self, node: &Node) -> bool {
        let client = node.client.and_then(|id| self.clients.get(&id));
        let pid = node.pid.or(client.and_then(|(pid, _)| *pid));
        let binary = node
            .binary
            .as_deref()
            .or(client.and_then(|(_, binary)| binary.as_deref()));
        pid == Some(self.our_pid) || binary.is_some_and(|binary| OUR_BINARIES.contains(&binary))
    }

    /// The connections that should exist and have not been asked for yet,
    /// as pairs of an output port and one of our input ports.
    fn missing(&self) -> Vec<(u64, u64)> {
        let target = self
            .nodes
            .iter()
            .find(|(_, node)| node.name == self.target)
            .map(|(id, _)| *id);
        let Some(target) = target else {
            return Vec::new();
        };
        let ours: Vec<(u64, &str)> = self
            .ports
            .iter()
            .filter(|(_, port)| port.node == target && !port.output)
            .map(|(id, port)| (*id, port.channel.as_str()))
            .collect();
        let mut missing = Vec::new();
        for (id, port) in &self.ports {
            let collect = port.output
                && self
                    .nodes
                    .get(&port.node)
                    .is_some_and(|node| node.playback && !self.ours(node));
            if !collect {
                continue;
            }
            // Left goes to left and right to right. What is meant for the
            // middle goes to both; the rear of a surround mix is left out.
            let wanted: &[&str] = match port.channel.as_str() {
                "FL" | "AUX0" => &["FL"],
                "FR" | "AUX1" => &["FR"],
                "MONO" | "FC" => &["FL", "FR"],
                _ => &[],
            };
            for (input, channel) in &ours {
                if wanted.contains(channel) && !self.connected.contains(&(*id, *input)) {
                    missing.push((*id, *input));
                }
            }
        }
        missing.sort_unstable();
        missing
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node(id: u64, name: &str, class: &str, pid: u64, binary: &str) -> Value {
        json!({ "id": id, "type": "PipeWire:Interface:Node", "info": { "props": {
            "node.name": name, "media.class": class,
            "application.process.id": pid, "application.process.binary": binary,
        }}})
    }

    fn port(id: u64, node: u64, direction: &str, channel: &str) -> Value {
        json!({ "id": id, "type": "PipeWire:Interface:Port", "info": {
            "direction": direction,
            "props": { "node.id": node, "audio.channel": channel },
        }})
    }

    /// A machine on which a game, a mono notification and our own call
    /// play, with our recording stream open.
    fn graph() -> Graph {
        let mut graph = Graph::new("capture", 500);
        for object in [
            node(10, "capture", "Stream/Input/Audio", 500, "flowerchat"),
            port(11, 10, "input", "FL"),
            port(12, 10, "input", "FR"),
            node(20, "game", "Stream/Output/Audio", 700, "game"),
            port(21, 20, "output", "FL"),
            port(22, 20, "output", "FR"),
            node(30, "bell", "Stream/Output/Audio", 701, "notify"),
            port(31, 30, "output", "MONO"),
            node(40, "call", "Stream/Output/Audio", 500, "flowerchat"),
            port(41, 40, "output", "FL"),
            node(50, "microphone", "Stream/Input/Audio", 702, "recorder"),
            port(51, 50, "input", "FL"),
        ] {
            graph.update(&object);
        }
        graph
    }

    #[test]
    fn everything_that_plays_is_collected_except_the_call() {
        let graph = graph();
        assert_eq!(
            graph.missing(),
            [(21, 11), (22, 12), (31, 11), (31, 12)],
            "the game in stereo, the bell on both sides, and not the call"
        );
    }

    #[test]
    fn another_copy_of_ourselves_is_left_out_too() {
        let mut graph = graph();
        graph.update(&node(
            60,
            "other call",
            "Stream/Output/Audio",
            900,
            "flowerchat",
        ));
        graph.update(&port(61, 60, "output", "FL"));
        assert!(graph.missing().iter().all(|(output, _)| *output != 61));
    }

    #[test]
    fn a_connection_is_asked_for_once_and_again_after_it_is_gone() {
        let mut graph = graph();
        for pair in graph.missing() {
            graph.connected.insert(pair);
        }
        assert!(graph.missing().is_empty());
        // The game closes its stream and opens a new one.
        graph.update(&json!({ "id": 21, "info": null }));
        graph.update(&port(23, 20, "output", "FL"));
        assert_eq!(graph.missing(), [(23, 11)]);
    }

    #[test]
    fn a_program_is_known_by_its_connection_when_its_stream_does_not_say() {
        let mut graph = graph();
        graph.update(
            &json!({ "id": 70, "type": "PipeWire:Interface:Client", "info": {
                "props": { "pipewire.sec.pid": 500, "application.process.binary": "flowerchat" },
            }}),
        );
        graph.update(
            &json!({ "id": 80, "type": "PipeWire:Interface:Node", "info": { "props": {
                "node.name": "ours", "media.class": "Stream/Output/Audio", "client.id": 70,
            }}}),
        );
        graph.update(&port(81, 80, "output", "FL"));
        assert!(graph.missing().iter().all(|(output, _)| *output != 81));
    }
}
