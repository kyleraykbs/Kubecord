//! Where a Go Live stream's sound comes from. The stream's audio is
//! recorded from a virtual output of kubecord's own (`kubecord Go Live`)
//! that nothing plays into by default. Depending on the choice, kubecord
//! then connects other programs' playback into it (everything playing to
//! the default output except kubecord, or one program), or leaves the
//! connecting to the user's patchbay.
//!
//! The PipeWire graph is read with `pw-dump` and connections are made with
//! `pw-link`; both come with PipeWire. A connection lasts until either end
//! goes away, so ending the capture removes them all.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};

/// Where the stream's sound comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioShare {
    /// Everything playing to the default output, except kubecord itself.
    Desktop,
    /// One program's playback, by [`AudioApp::key`].
    Application { key: String },
    /// Nothing until connected by hand in a patchbay.
    Node,
}

/// A program with sound, as offered for [`AudioShare::Application`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    /// What identifies its streams: the program's file name, or its
    /// application name when PipeWire does not know the file.
    pub key: String,
    /// The name it gives itself.
    pub name: String,
}

/// The node name under which the capture appears when left for a
/// patchbay. It stays the same from stream to stream so a patchbay can
/// remember its connections.
pub const NODE_NAME: &str = "kubecord-go-live";
/// The name a patchbay shows for the capture.
pub const NODE_DESCRIPTION: &str = "kubecord Go Live";

/// How often the graph is checked for streams that need connecting.
const LINK_INTERVAL: Duration = Duration::from_millis(1500);

/// One port in the graph.
#[derive(Clone, Debug)]
struct Port {
    id: u32,
    node: u32,
    output: bool,
    channel: Option<String>,
    monitor: bool,
}

/// What `pw-dump` says about the graph, as far as linking needs it.
#[derive(Debug, Default)]
struct Graph {
    nodes: HashMap<u32, serde_json::Map<String, Value>>,
    ports: Vec<Port>,
    /// Existing links, as (output port, input port).
    links: HashSet<(u32, u32)>,
    /// Existing links, as (output node, input node).
    node_links: HashSet<(u32, u32)>,
    /// The node name of the default output.
    default_sink: Option<String>,
}

/// A property as text, whether PipeWire wrote it as a string or a number.
fn text(props: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match props.get(key)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn number(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|n| u32::try_from(n).ok())
}

impl Graph {
    fn parse(json: &str) -> Result<Self> {
        let objects: Vec<Value> = serde_json::from_str(json)
            .map_err(|error| Error::Message(format!("unreadable pw-dump output: {error}")))?;
        let mut graph = Self::default();
        for object in &objects {
            let Some(id) = object.get("id").and_then(number) else {
                continue;
            };
            let info = object.get("info");
            // Nodes and ports keep their properties under `info`; metadata
            // keeps them at the top.
            let props = info
                .and_then(|info| info.get("props"))
                .or_else(|| object.get("props"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            match object.get("type").and_then(Value::as_str) {
                Some("PipeWire:Interface:Node") => {
                    graph.nodes.insert(id, props);
                }
                Some("PipeWire:Interface:Port") => {
                    let Some(node) = props.get("node.id").and_then(number) else {
                        continue;
                    };
                    let direction = info
                        .and_then(|info| info.get("direction"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| text(&props, "port.direction"))
                        .unwrap_or_default();
                    graph.ports.push(Port {
                        id,
                        node,
                        output: direction == "output" || direction == "out",
                        channel: text(&props, "audio.channel"),
                        monitor: props
                            .get("port.monitor")
                            .is_some_and(|monitor| monitor.as_bool() == Some(true)),
                    });
                }
                Some("PipeWire:Interface:Link") => {
                    let field = |key: &str| info.and_then(|info| info.get(key)).and_then(number);
                    if let (Some(out_port), Some(in_port)) =
                        (field("output-port-id"), field("input-port-id"))
                    {
                        graph.links.insert((out_port, in_port));
                    }
                    if let (Some(out_node), Some(in_node)) =
                        (field("output-node-id"), field("input-node-id"))
                    {
                        graph.node_links.insert((out_node, in_node));
                    }
                }
                Some("PipeWire:Interface:Metadata") => {
                    if text(&props, "metadata.name").as_deref() != Some("default") {
                        continue;
                    }
                    let entries = object.get("metadata").and_then(Value::as_array);
                    for entry in entries.into_iter().flatten() {
                        if entry.get("key").and_then(Value::as_str) != Some("default.audio.sink") {
                            continue;
                        }
                        // The value is `{"name": …}`, sometimes as JSON text.
                        let value = entry.get("value");
                        let name = match value {
                            Some(Value::Object(map)) => {
                                map.get("name").and_then(Value::as_str).map(str::to_owned)
                            }
                            Some(Value::String(json)) => {
                                serde_json::from_str::<Value>(json).ok().and_then(|value| {
                                    value.get("name").and_then(Value::as_str).map(str::to_owned)
                                })
                            }
                            _ => None,
                        };
                        if name.is_some() {
                            graph.default_sink = name;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(graph)
    }

    fn class(&self, node: u32) -> Option<String> {
        self.nodes
            .get(&node)
            .and_then(|props| text(props, "media.class"))
    }

    /// Playback streams of programs (not devices, not other captures).
    fn playback_streams(&self) -> impl Iterator<Item = (u32, &serde_json::Map<String, Value>)> {
        self.nodes.iter().filter_map(|(id, props)| {
            (text(props, "media.class").as_deref() == Some("Stream/Output/Audio"))
                .then_some((*id, props))
        })
    }

    /// The output (sink) node named `name`.
    /// The connections into the output named `name`, as pairs of ports.
    fn links_into(&self, name: &str) -> Vec<(u32, u32)> {
        let Some(node) = self.output_named(name) else {
            return Vec::new();
        };
        let inputs: HashSet<u32> = self
            .ports
            .iter()
            .filter(|port| port.node == node && !port.output)
            .map(|port| port.id)
            .collect();
        let mut links: Vec<(u32, u32)> = self
            .links
            .iter()
            .copied()
            .filter(|(_, input)| inputs.contains(input))
            .collect();
        links.sort_unstable();
        links
    }

    fn output_named(&self, name: &str) -> Option<u32> {
        self.nodes.iter().find_map(|(id, props)| {
            (text(props, "node.name").as_deref() == Some(name)
                && text(props, "media.class").as_deref() == Some("Audio/Sink"))
            .then_some(*id)
        })
    }
}

/// What identifies a program's streams.
fn app_key(props: &serde_json::Map<String, Value>) -> Option<String> {
    text(props, "application.process.binary").or_else(|| text(props, "application.name"))
}

/// Whether a stream is kubecord's: this process (`pid`), or another copy
/// of kubecord. Its sound is the call, which viewers must not hear back.
fn is_own(props: &serde_json::Map<String, Value>, pid: u32) -> bool {
    text(props, "application.process.id").is_some_and(|id| id == pid.to_string())
        || text(props, "application.name").as_deref() == Some("kubecord")
}

/// Programs with a playback stream, kubecord aside. Only streams that
/// name a process count, which leaves out helpers such as a patchbay's
/// keep-alive players.
fn applications_in(graph: &Graph, own_pid: u32) -> Vec<AudioApp> {
    let mut apps: Vec<AudioApp> = Vec::new();
    for (_, props) in graph.playback_streams() {
        if is_own(props, own_pid) || text(props, "application.process.id").is_none() {
            continue;
        }
        let Some(key) = app_key(props) else {
            continue;
        };
        if apps.iter().any(|app| app.key == key) {
            continue;
        }
        let name = text(props, "application.name").unwrap_or_else(|| key.clone());
        apps.push(AudioApp { key, name });
    }
    apps.sort_by_key(|app| app.name.to_lowercase());
    apps
}

/// The input channels a source channel feeds: left to left, right to
/// right, a single or centre channel to both.
fn targets(channel: Option<&str>, index: usize, count: usize) -> &'static [&'static str] {
    match channel {
        Some("FL" | "SL" | "RL" | "FLC" | "FLW" | "TFL") => &["FL"],
        Some("FR" | "SR" | "RR" | "FRC" | "FRW" | "TFR") => &["FR"],
        Some("MONO" | "FC") => &["FL", "FR"],
        Some("LFE") => &[],
        _ if count == 1 => &["FL", "FR"],
        _ => match index {
            0 => &["FL"],
            1 => &["FR"],
            _ => &[],
        },
    }
}

/// The links still to make into the capture node `node_name`.
fn links_to_make(
    graph: &Graph,
    node_name: &str,
    share: &AudioShare,
    own_pid: u32,
) -> Vec<(u32, u32)> {
    let Some(capture) = graph.output_named(node_name) else {
        return Vec::new();
    };
    let inputs: HashMap<String, u32> = graph
        .ports
        .iter()
        .filter(|port| port.node == capture && !port.output)
        .filter_map(|port| Some((port.channel.clone()?, port.id)))
        .collect();
    if inputs.is_empty() {
        return Vec::new();
    }

    // The default output, to tell desktop sound from other playback.
    let default_sink = graph.default_sink.as_ref().and_then(|name| {
        graph.nodes.iter().find_map(|(id, props)| {
            (text(props, "node.name").as_ref() == Some(name)).then_some(*id)
        })
    });
    let plays_to_desktop = |stream: u32| {
        graph.node_links.iter().any(|(from, to)| {
            *from == stream
                && match default_sink {
                    Some(sink) => *to == sink,
                    None => graph.class(*to).as_deref() == Some("Audio/Sink"),
                }
        })
    };

    let mut sources: Vec<u32> = graph
        .playback_streams()
        .filter(|(id, props)| {
            !is_own(props, own_pid)
                && match share {
                    AudioShare::Desktop => plays_to_desktop(*id),
                    AudioShare::Application { key } => app_key(props).as_ref() == Some(key),
                    AudioShare::Node => false,
                }
        })
        .map(|(id, _)| id)
        .collect();
    sources.sort_unstable();

    let mut links = Vec::new();
    for source in sources {
        let mut outputs: Vec<&Port> = graph
            .ports
            .iter()
            .filter(|port| port.node == source && port.output && !port.monitor)
            .collect();
        outputs.sort_by_key(|port| port.id);
        let count = outputs.len();
        for (index, port) in outputs.into_iter().enumerate() {
            for channel in targets(port.channel.as_deref(), index, count) {
                if let Some(input) = inputs.get(*channel) {
                    let link = (port.id, *input);
                    if !graph.links.contains(&link) {
                        links.push(link);
                    }
                }
            }
        }
    }
    links
}

/// An output other programs can play into, like a speaker, that exists
/// while this lives. PipeWire keeps a node only as long as the program
/// that made it, so a `pw-cli` started by kubecord holds it. That `pw-cli`
/// is stopped through its own handle when this is dropped, and by the
/// kernel if kubecord itself ends, so the output never outlives kubecord.
pub struct VirtualOutput {
    name: String,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VirtualOutput {
    /// Creates the output named `name` (shown as [`NODE_DESCRIPTION`]) and
    /// waits until it is there.
    pub fn create(name: &str) -> Result<Self> {
        use std::io::Write;
        use std::os::unix::process::CommandExt;

        let command = format!(
            "create-node adapter {{ factory.name=support.null-audio-sink \
             node.name={name} node.description=\"{NODE_DESCRIPTION}\" \
             media.class=Audio/Sink audio.position=[ FL FR ] \
             priority.session=0 priority.driver=0 }}\n"
        );
        let (started_tx, started_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        // The thread that starts `pw-cli` lives exactly as long as it: the
        // kernel's "stop when the parent goes" applies to the starting
        // thread, not the whole process.
        let thread = std::thread::spawn(move || {
            let mut cli = Command::new("pw-cli");
            cli.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // SAFETY: prctl only sets a flag on the child about to run; it
            // touches no memory of ours.
            unsafe {
                cli.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let mut child = match cli.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = started_tx.send(Err(Error::Message(format!(
                        "could not run pw-cli: {error}"
                    ))));
                    return;
                }
            };
            // Kept open: pw-cli waits on it for further commands.
            let mut input = child.stdin.take();
            let sent = input.as_mut().map(|input| {
                input
                    .write_all(command.as_bytes())
                    .and_then(|_| input.flush())
            });
            let _ = started_tx.send(match sent {
                Some(Ok(())) => Ok(()),
                _ => Err(Error::Message("could not talk to pw-cli".into())),
            });
            let _ = stop_rx.recv();
            let _ = child.kill();
            let _ = child.wait();
            drop(input);
        });
        let output = Self {
            name: name.to_owned(),
            stop: Some(stop_tx),
            thread: Some(thread),
        };
        started_rx
            .recv()
            .map_err(|_| Error::Message("pw-cli ended at once".into()))??;
        // Wait for PipeWire to show it before anything records from it.
        for _ in 0..30 {
            if dump().is_ok_and(|graph| graph.output_named(name).is_some()) {
                tracing::info!(name, "virtual output ready");
                return Ok(output);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(Error::Message(format!(
            "PipeWire did not show the {NODE_DESCRIPTION} output"
        )))
    }
}

/// How long the session manager gets to notice that an output has been
/// disconnected before the output is removed.
const DISCONNECT_SETTLE: Duration = Duration::from_millis(250);

/// Disconnects everything from the output named `name`; returns how many
/// connections there were.
fn disconnect(name: &str) -> usize {
    let links = dump().map_or_else(|_| Vec::new(), |graph| graph.links_into(name));
    for (output, input) in &links {
        let _ = Command::new("pw-link")
            .arg("--disconnect")
            .arg(output.to_string())
            .arg(input.to_string())
            .output();
    }
    links.len()
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        // WirePlumber pauses media players (over MPRIS) when an output
        // their sound is connected to is removed, as it would for
        // unplugged headphones: ending a stream paused the video playing
        // in a browser. An output nothing is connected to pauses nothing,
        // so everything is disconnected first.
        if disconnect(&self.name) > 0 {
            std::thread::sleep(DISCONNECT_SETTLE);
        }
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads the PipeWire graph.
fn dump() -> Result<Graph> {
    let output = Command::new("pw-dump")
        .arg("--no-colors")
        .output()
        .map_err(|error| Error::Message(format!("could not run pw-dump: {error}")))?;
    if !output.status.success() {
        return Err(Error::Message(format!(
            "pw-dump failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Graph::parse(&String::from_utf8_lossy(&output.stdout))
}

/// Whether the PipeWire tools this needs are installed.
pub fn tools_available() -> bool {
    ["pw-cli", "pw-dump", "pw-link"].iter().all(|tool| {
        Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// The programs that have sound right now, for choosing one to share.
pub fn applications() -> Result<Vec<AudioApp>> {
    Ok(applications_in(&dump()?, std::process::id()))
}

/// Keeps other programs' playback connected to a capture node while it
/// runs: streams that start later are connected as they appear.
pub(crate) struct Linker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Linker {
    pub(crate) fn start(node_name: String, share: AudioShare) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let own_pid = std::process::id();
            let mut first = true;
            let mut warned = false;
            while !thread_stop.load(Ordering::Relaxed) {
                // The node takes a moment to appear after the capture starts.
                std::thread::sleep(if first {
                    Duration::from_millis(300)
                } else {
                    LINK_INTERVAL
                });
                first = false;
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let graph = match dump() {
                    Ok(graph) => graph,
                    Err(error) => {
                        if !warned {
                            tracing::warn!(%error, "could not read the PipeWire graph");
                            warned = true;
                        }
                        continue;
                    }
                };
                for (output, input) in links_to_make(&graph, &node_name, &share, own_pid) {
                    let result = Command::new("pw-link")
                        .arg(output.to_string())
                        .arg(input.to_string())
                        .output();
                    match result {
                        Ok(result) if result.status.success() => {
                            tracing::info!(output, input, "connected sound into the stream");
                        }
                        Ok(result) => tracing::debug!(
                            output,
                            input,
                            error = %String::from_utf8_lossy(&result.stderr).trim(),
                            "could not connect sound into the stream"
                        ),
                        Err(error) => tracing::debug!(%error, "could not run pw-link"),
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Linker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWN_PID: u32 = 4242;

    fn node(id: u32, props: Value) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {"props": props}})
    }

    fn port(id: u32, node: u32, direction: &str, channel: &str) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Port", "info": {
            "direction": direction,
            "props": {"node.id": node, "audio.channel": channel}
        }})
    }

    fn link(id: u32, out_node: u32, out_port: u32, in_node: u32, in_port: u32) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Link", "info": {
            "output-node-id": out_node, "output-port-id": out_port,
            "input-node-id": in_node, "input-port-id": in_port
        }})
    }

    /// A desk: speakers (the default output), a patchbay's own sink, a
    /// browser and two copies of kubecord playing to the speakers, a keep-alive player
    /// feeding the patchbay, a mono game not yet connected, and our
    /// capture node.
    fn desk() -> Graph {
        let objects = json!([
            {"id": 0, "type": "PipeWire:Interface:Metadata",
             "props": {"metadata.name": "default"},
             "metadata": [{"subject": 0, "key": "default.audio.sink",
                           "type": "Spa:String:JSON", "value": {"name": "speakers"}}]},
            node(10, json!({"media.class": "Audio/Sink", "node.name": "speakers"})),
            port(11, 10, "input", "FL"), port(12, 10, "input", "FR"),
            node(20, json!({"media.class": "Audio/Sink", "node.name": "patch-sink"})),
            port(21, 20, "input", "FL"), port(22, 20, "input", "FR"),
            node(30, json!({"media.class": "Stream/Output/Audio", "node.name": "Browser",
                            "application.name": "Browser", "application.process.binary": "browser",
                            "application.process.id": 100})),
            port(31, 30, "output", "FL"), port(32, 30, "output", "FR"),
            link(33, 30, 31, 10, 11), link(34, 30, 32, 10, 12),
            node(40, json!({"media.class": "Stream/Output/Audio", "node.name": "kubecord",
                            "application.name": "kubecord", "application.process.id": OWN_PID})),
            port(41, 40, "output", "FL"), port(42, 40, "output", "FR"),
            link(43, 40, 41, 10, 11), link(44, 40, 42, 10, 12),
            // Another copy of kubecord, also playing to the speakers.
            node(45, json!({"media.class": "Stream/Output/Audio", "node.name": "kubecord",
                            "application.name": "kubecord", "application.process.id": 999})),
            port(46, 45, "output", "FL"), port(47, 45, "output", "FR"),
            link(48, 45, 46, 10, 11), link(49, 45, 47, 10, 12),
            node(50, json!({"media.class": "Stream/Output/Audio", "node.name": "x_keepalive",
                            "application.name": "pw-cat"})),
            port(51, 50, "output", "FL"),
            link(53, 50, 51, 20, 21),
            node(60, json!({"media.class": "Stream/Output/Audio", "node.name": "Game",
                            "application.name": "Game", "application.process.id": "200"})),
            port(61, 60, "output", "MONO"),
            node(70, json!({"media.class": "Audio/Sink", "node.name": "kubecord-go-live-1"})),
            port(71, 70, "input", "FL"), port(72, 70, "input", "FR"),
            port(73, 70, "output", "FL"),
        ]);
        Graph::parse(&objects.to_string()).unwrap()
    }

    #[test]
    fn everything_connected_to_the_output_is_found_for_disconnecting() {
        // Nothing connected yet.
        assert!(desk().links_into("kubecord-go-live-1").is_empty());
        // The browser connected to it, as well as to the speakers; the
        // output's own monitor port feeding a recorder is not a connection
        // into it.
        let objects = json!([
            node(
                10,
                json!({"media.class": "Audio/Sink", "node.name": "speakers"})
            ),
            port(11, 10, "input", "FL"),
            node(
                30,
                json!({"media.class": "Stream/Output/Audio", "node.name": "Browser"})
            ),
            port(31, 30, "output", "FL"),
            port(32, 30, "output", "FR"),
            link(33, 30, 31, 10, 11),
            node(
                70,
                json!({"media.class": "Audio/Sink", "node.name": "kubecord-go-live-1"})
            ),
            port(71, 70, "input", "FL"),
            port(72, 70, "input", "FR"),
            port(73, 70, "output", "FL"),
            link(80, 30, 31, 70, 71),
            link(81, 30, 32, 70, 72),
            node(
                90,
                json!({"media.class": "Stream/Input/Audio", "node.name": "recorder"})
            ),
            port(91, 90, "input", "FL"),
            link(92, 70, 73, 90, 91),
        ]);
        let graph = Graph::parse(&objects.to_string()).unwrap();
        assert_eq!(
            graph.links_into("kubecord-go-live-1"),
            vec![(31, 71), (32, 72)]
        );
        assert!(graph.links_into("some-other-output").is_empty());
    }

    #[test]
    fn desktop_audio_is_what_plays_to_the_default_output_except_kubecord() {
        let graph = desk();
        assert_eq!(graph.default_sink.as_deref(), Some("speakers"));
        let links = links_to_make(&graph, "kubecord-go-live-1", &AudioShare::Desktop, OWN_PID);
        // The browser, left to left and right to right; not kubecord, not
        // the keep-alive feeding the patchbay, not the unconnected game.
        assert_eq!(links, vec![(31, 71), (32, 72)]);
    }

    #[test]
    fn one_program_is_connected_wherever_it_plays() {
        let graph = desk();
        let game = AudioShare::Application {
            key: "Game".to_owned(),
        };
        // A mono stream feeds both sides.
        assert_eq!(
            links_to_make(&graph, "kubecord-go-live-1", &game, OWN_PID),
            vec![(61, 71), (61, 72)]
        );
        let browser = AudioShare::Application {
            key: "browser".to_owned(),
        };
        assert_eq!(
            links_to_make(&graph, "kubecord-go-live-1", &browser, OWN_PID),
            vec![(31, 71), (32, 72)]
        );
    }

    #[test]
    fn links_already_made_are_not_made_again() {
        let mut graph = desk();
        graph.links.insert((31, 71));
        let links = links_to_make(&graph, "kubecord-go-live-1", &AudioShare::Desktop, OWN_PID);
        assert_eq!(links, vec![(32, 72)]);
        // A patchbay node is left alone, and so is a node that is not there.
        assert!(links_to_make(&graph, "kubecord-go-live-1", &AudioShare::Node, OWN_PID).is_empty());
        assert!(links_to_make(&graph, "missing", &AudioShare::Desktop, OWN_PID).is_empty());
    }

    #[test]
    fn the_program_list_leaves_out_kubecord_and_helpers() {
        let apps = applications_in(&desk(), OWN_PID);
        let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
        assert_eq!(names, ["Browser", "Game"]);
        assert_eq!(apps[0].key, "browser");
        // Without a file name, the application name identifies it.
        assert_eq!(apps[1].key, "Game");
    }

    #[test]
    fn channels_map_onto_left_and_right() {
        assert_eq!(targets(Some("FL"), 0, 2), ["FL"]);
        assert_eq!(targets(Some("RR"), 3, 6), ["FR"]);
        assert_eq!(targets(Some("FC"), 2, 6), ["FL", "FR"]);
        assert!(targets(Some("LFE"), 3, 6).is_empty());
        // Unnamed channels go by position.
        assert_eq!(targets(None, 0, 1), ["FL", "FR"]);
        assert_eq!(targets(None, 1, 2), ["FR"]);
    }
}
