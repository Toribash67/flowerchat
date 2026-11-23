mod call;
mod ice;
mod latency;
mod signaling;

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use flowerchat_protocol::{ClientMsg, PeerId, ServerMsg};
use futures_util::StreamExt;
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::mpsc;

use crate::call::{Call, CallConfig, CallEvent};
use crate::ice::IceConfig;
use crate::latency::Summary;

/// Voice chat client for flowerchat.
#[derive(Parser)]
#[command(name = "flowerchat", version)]
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
    gst::init()?;

    let (welcome, mut to_server, mut from_server) =
        signaling::join(&args.server, &args.room, &args.name).await?;
    let my_id = welcome.id;
    println!("joined {} as {} (peer {my_id})", args.room, args.name);

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let call = Call::new(
        CallConfig {
            input: args.input,
            output: args.output,
            jitter_ms: args.jitter_ms,
            raw_mic: args.raw_mic,
            test_tone: args.test_tone,
            measure: args.measure,
            ice: IceConfig::from_servers(&welcome.ice_servers),
        },
        events_tx,
    )?;
    let mut bus = call.bus().stream();

    // The side with the lower id makes the offer. Ids only grow, so that is
    // always whoever was in the room first.
    let add_peer = |id: PeerId, name: &str| {
        println!("{name} (peer {id}) is in the room");
        if let Err(err) = call.add_peer(id, name, my_id < id) {
            eprintln!("cannot connect to {name}: {err:#}");
        }
    };
    for peer in &welcome.peers {
        add_peer(peer.id, &peer.name);
    }

    let deadline = async {
        match args.duration {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);

    let mut summary = Summary::default();
    let outcome = loop {
        tokio::select! {
            msg = from_server.next() => match msg {
                Ok(Some(ServerMsg::PeerJoined { id, name })) => add_peer(id, &name),
                Ok(Some(ServerMsg::PeerLeft { id })) => {
                    println!("peer {id} left");
                    call.remove_peer(id);
                }
                Ok(Some(ServerMsg::Signal { from, data })) => {
                    if let Err(err) = call.on_signal(from, &data) {
                        eprintln!("bad signaling from peer {from}: {err:#}");
                    }
                }
                Ok(Some(ServerMsg::Error { message })) => break Err(anyhow::anyhow!("server: {message}")),
                Ok(Some(ServerMsg::Welcome { .. })) => {}
                Ok(None) => break Err(anyhow::anyhow!("the signaling server disconnected")),
                Err(err) => break Err(err),
            },
            Some(event) = events.recv() => match event {
                CallEvent::Signal { to, data } => {
                    if let Err(err) = to_server.send(&ClientMsg::Signal { to, data }).await {
                        break Err(err);
                    }
                }
                CallEvent::PeerState { id, state } => println!("peer {id}: {state}"),
                CallEvent::PeerMuted { id, muted } => {
                    println!("peer {id} {}", if muted { "muted" } else { "unmuted" });
                }
                CallEvent::ClickDelay(delay_ns) => {
                    summary.add(delay_ns);
                    println!("click delay {:.1} ms", delay_ns as f64 / 1e6);
                }
            },
            Some(message) = bus.next() => match message.view() {
                gst::MessageView::Error(err) => {
                    let source = err.src().map(|s| s.path_string()).unwrap_or_default();
                    break Err(anyhow::anyhow!(
                        "audio pipeline failed in {source}: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    ));
                }
                gst::MessageView::Latency(_) => call.recalculate_latency(),
                _ => {}
            },
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = &mut deadline => break Ok(()),
        }
    };
    call.shutdown();

    if let Some((min, median, max, count)) = summary.stats() {
        println!(
            "latency: median {median:.1} ms, min {min:.1} ms, max {max:.1} ms over {count} clicks"
        );
    } else if args.measure && outcome.is_ok() {
        bail!("no clicks were received; is a --test-tone sender in the room?");
    }
    outcome
}
