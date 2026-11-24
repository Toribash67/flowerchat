//! Collecting what plays on Linux, through PipeWire.
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

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use serde_json::Value;
use tracing::{debug, warn};

/// Programs whose playback is never collected: other copies of ourselves on
/// this machine, which play the same call.
const OUR_BINARIES: [&str; 2] = ["flowerchat", "flowerchat-cli"];

/// The element that records what is collected, named `capture`.
pub(super) fn description() -> String {
    "pipewiresrc name=capture autoconnect=false provide-clock=false \
     do-timestamp=true client-name=flowerchat"
        .to_owned()
}

/// Starts collecting into `capture`, for as long as what is returned lives.
pub(super) fn collect(capture: &gst::Element) -> Result<Collector> {
    let node = unique_node_name();
    let properties = gst::Structure::builder("props")
        .field("node.name", node.as_str())
        .field("node.description", "flowerchat stream sound")
        .field("media.class", "Stream/Input/Audio")
        .build();
    capture.set_property("stream-properties", properties);
    Collector::start(&node)
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
pub(super) struct Collector {
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
