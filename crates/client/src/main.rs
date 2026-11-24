//! Command-line client: joins a room and talks, with no window. Also hosts
//! the test modes used for measuring latency.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use flowerchat_client::latency::Summary;
use flowerchat_client::session::{
    self, AudioSettings, Command, Event, ScreenSource, SessionConfig, ShareSettings, ShareSource,
};
use flowerchat_protocol::PeerId;
use tokio::sync::mpsc::unbounded_channel;

/// Stands in for "never" when no --duration is given.
const FOREVER: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// Voice chat client for flowerchat, without a window.
#[derive(Parser)]
#[command(name = "flowerchat-cli", version)]
struct Args {
    /// Name shown to the others in the room.
    #[arg(long)]
    name: String,
    /// Room to join.
    #[arg(long, default_value = "lounge")]
    room: String,
    /// WebSocket address of the signaling server.
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    server: String,
    /// Microphone to use, as a PipeWire/PulseAudio source name.
    #[arg(long)]
    input: Option<String>,
    /// Speakers to use, as a PipeWire/PulseAudio sink name.
    #[arg(long)]
    output: Option<String>,
    /// Receive buffer in milliseconds. Lower means less delay, but audio
    /// that arrives later than this is lost.
    #[arg(long, default_value_t = 40)]
    jitter_ms: u32,
    /// Send the microphone without echo cancellation, noise suppression or
    /// automatic gain.
    #[arg(long)]
    raw_mic: bool,
    /// Send one click per second instead of the microphone.
    #[arg(long)]
    test_tone: bool,
    /// Do not play anything; report how long clicks from a --test-tone
    /// sender on this machine took to arrive.
    #[arg(long)]
    measure: bool,
    /// Print each connection's statistics once a second.
    #[arg(long)]
    stats: bool,
    /// Send this chat message to each peer once connected to it.
    #[arg(long)]
    say: Option<String>,
    /// Share a moving test picture of this size, such as 1920x1080, as if
    /// it were a screen.
    #[arg(long, value_name = "WIDTHxHEIGHT")]
    share_test: Option<String>,
    /// Share a PipeWire video stream that is open to everyone, by its node
    /// id. Screens are not; the app asks the desktop for those.
    #[arg(long, value_name = "ID", conflicts_with = "share_test")]
    share_node: Option<u32>,
    /// Scale the shared picture down to this height if it is taller.
    #[arg(long)]
    share_height: Option<u32>,
    /// Frames per second to share at.
    #[arg(long, default_value_t = 60)]
    share_fps: u32,
    /// Watch every screen that is shared, without showing it, and print how
    /// it goes once a second.
    #[arg(long)]
    watch: bool,
    /// For testing: lose this many percent of the packets that arrive, as
    /// a poor network would.
    #[arg(long, default_value_t = 0.0)]
    lose: f64,
    /// Leave after this many seconds.
    #[arg(long)]
    duration: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    gstreamer::init()?;

    let audio = AudioSettings {
        input: args.input,
        output: args.output,
        jitter_ms: args.jitter_ms,
        ..AudioSettings::default()
    };
    let audio = if args.raw_mic {
        audio.unprocessed()
    } else {
        audio
    };

    let source = match (&args.share_test, args.share_node) {
        (Some(size), _) => {
            let (width, height) = flowerchat_client::video::parse_size(size)
                .with_context(|| format!("{size:?} is not a size like 1920x1080"))?;
            Some(ShareSource::TestPicture { width, height })
        }
        (None, Some(node)) => Some(ShareSource::Screen(ScreenSource { remote: None, node })),
        (None, None) => None,
    };
    let share = source.map(|source| Command::StartShare {
        source,
        settings: ShareSettings {
            max_height: args.share_height,
            fps: args.share_fps,
        },
    });

    let (commands, command_rx) = unbounded_channel();
    let (event_tx, mut events) = unbounded_channel();
    let session = tokio::spawn(session::run(
        SessionConfig {
            server: args.server,
            room: args.room.clone(),
            name: args.name.clone(),
            audio,
            transmitting: true,
            test_tone: args.test_tone,
            measure: args.measure,
            frames: None,
            incoming_loss: args.lose / 100.0,
        },
        command_rx,
        event_tx,
    ));

    let deadline = tokio::time::sleep(args.duration.map_or(FOREVER, Duration::from_secs));
    tokio::pin!(deadline);

    let mut names: HashMap<PeerId, String> = HashMap::new();
    let mut levels: HashMap<PeerId, f32> = HashMap::new();
    let mut sharers: HashSet<PeerId> = HashSet::new();
    let mut mic_level = f32::NEG_INFINITY;
    let mut summary = Summary::default();
    let mut leaving = false;
    loop {
        tokio::select! {
            event = events.recv() => {
                // The channel closes when the session ends, for any reason.
                let Some(event) = event else { break };
                match event {
                    Event::Joined { id } => {
                        println!("joined {} as {} (peer {id})", args.room, args.name);
                        if let Some(share) = share.clone() {
                            let _ = commands.send(share);
                        }
                    }
                    Event::PeerJoined { id, name } => {
                        println!("{name} (peer {id}) is in the room");
                        names.insert(id, name);
                    }
                    Event::PeerLeft { id } => println!("peer {id} left"),
                    Event::PeerState { id, state } => {
                        println!("peer {id}: {}", format!("{state:?}").to_lowercase());
                    }
                    Event::PeerMuted { id, muted } => {
                        println!("peer {id} {}", if muted { "muted" } else { "unmuted" });
                        // The peer's first status message shows that its
                        // control channel is open, so chat can reach it.
                        if let Some(text) = &args.say {
                            let _ = commands.send(Command::SendChat(text.clone()));
                        }
                    }
                    Event::Chat { from, text } => {
                        let name = names.get(&from).map(String::as_str).unwrap_or("?");
                        println!("<{name}> {text}");
                    }
                    Event::PeerStats { id, stats } if args.stats => {
                        let level = levels.get(&id).copied().unwrap_or(f32::NEG_INFINITY);
                        println!("peer {id} level {level:.0} dB, mic {mic_level:.0} dB, {stats:?}");
                    }
                    Event::PeerLevel { id, db } => {
                        levels.insert(id, db);
                    }
                    Event::MicLevel { db } => mic_level = db,
                    Event::ClickDelay(delay_ns) => {
                        summary.add(delay_ns);
                        println!("click delay {:.1} ms", delay_ns as f64 / 1e6);
                    }
                    Event::PeerStats { .. } => {}
                    Event::OwnShare(Some(sharing)) => println!(
                        "sharing {}x{} at {} fps, {} kbps: sent {:.1} fps, encoding takes {:.1} ms, watched by {:?}",
                        sharing.width,
                        sharing.height,
                        sharing.fps,
                        sharing.kbps,
                        sharing.sent_fps,
                        sharing.encode_ms,
                        sharing.watchers
                    ),
                    Event::OwnShare(None) => println!("stopped sharing"),
                    Event::ShareFailed(reason) => println!("sharing failed: {reason}"),
                    Event::PeerSharing { id, sharing } => {
                        let was_sharing = sharers.contains(&id);
                        match sharing {
                            Some(sharing) if !was_sharing => {
                                println!(
                                    "peer {id} shares {}x{} at {} fps, {} kbps",
                                    sharing.width, sharing.height, sharing.fps, sharing.kbps
                                );
                                sharers.insert(id);
                                if args.watch {
                                    let _ = commands.send(Command::Watch { id, watch: true });
                                }
                            }
                            None if was_sharing => {
                                println!("peer {id} stopped sharing");
                                sharers.remove(&id);
                            }
                            _ => {}
                        }
                    }
                    Event::Watching { id, stats } => println!(
                        "watching peer {id}: {}x{} at {:.1} fps, {:.1}% dropped, ready in {:.1} ms",
                        stats.width, stats.height, stats.fps, stats.dropped_percent, stats.decode_ms
                    ),
                    Event::WatchFailed { id, reason } => {
                        println!("cannot watch peer {id}: {reason}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c(), if !leaving => {
                leaving = true;
                let _ = commands.send(Command::Leave);
            }
            _ = &mut deadline, if !leaving => {
                leaving = true;
                let _ = commands.send(Command::Leave);
            }
        }
    }
    let outcome = session.await?;

    if let Some((min, median, max, count)) = summary.stats() {
        println!(
            "latency: median {median:.1} ms, min {min:.1} ms, max {max:.1} ms over {count} clicks"
        );
    } else if args.measure && outcome.is_ok() {
        bail!("no clicks were received; is a --test-tone sender in the room?");
    }
    outcome
}
