//! Shares at a high bitrate while someone watches, changing bitrate and
//! codec over and over, and checks after every change that frames keep
//! arriving.
//!
//! Takes minutes, so it only runs when asked for:
//! `cargo test -p flowerchat-client --test stress -- --ignored --nocapture`
//!
//! Set through the environment:
//! - `STRESS_ROUNDS`, `STRESS_DWELL_MS`, `STRESS_SEED`: how many changes,
//!   how long each is watched, and which sequence of changes.
//! - `STRESS_TOP_KBPS`, `STRESS_FPS`, `STRESS_HEVC=1`: the share.
//! - `STRESS_HOLD=1`: change nothing, only watch.
//! - `STRESS_NODE=<id>`: share that PipeWire node in place of the test
//!   picture, which takes the same path as a real screen. One that delivers
//!   frames faster than they are encoded, as a busy 170 Hz screen does, is
//!   what used to lock `pipewiresrc` up:
//!   `gst-launch-1.0 videotestsrc is-live=true pattern=black !
//!   video/x-raw,format=BGRx,width=2560,height=1440,framerate=170/1 !
//!   pipewiresink mode=provide
//!   stream-properties="props,media.class=Video/Source,node.name=fake"`
//!   (find its id with `pw-dump`; start it anew for every run).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flowerchat_client::session::{
    self, AudioSettings, Codec, Command, Event, FrameSink, ScreenSource, SessionConfig,
    ShareSettings, ShareSource, VideoFrame,
};
use flowerchat_protocol::PeerId;
use flowerchat_server::{Config, app};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

#[derive(Default)]
struct Seen {
    id: Option<PeerId>,
    frames: u64,
    video_kbps: Option<f64>,
    video_loss: Option<f64>,
    problems: Vec<String>,
    /// What the sharer says of its own share: frames sent per second and
    /// the time encoding takes.
    own: Option<(f64, f64)>,
    /// Share of decoded frames the viewer did not get to show, and the time
    /// from arrival to being shown.
    watch: Option<(f64, f64)>,
}

struct Client {
    commands: UnboundedSender<Command>,
    seen: Arc<Mutex<Seen>>,
}

fn join(url: &str, name: &str) -> Client {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let frame_log = seen.clone();
    let frames: FrameSink = Arc::new(move |_, _: VideoFrame| {
        frame_log.lock().unwrap().frames += 1;
    });
    let (commands, command_rx) = unbounded_channel();
    let (event_tx, mut events) = unbounded_channel();
    let config = SessionConfig {
        server: url.into(),
        room: "lounge".into(),
        name: name.into(),
        audio: AudioSettings::default(),
        transmitting: true,
        test_tone: true,
        measure: true,
        frames: Some(frames),
        incoming_loss: 0.0,
    };
    tokio::spawn(session::run(config, command_rx, event_tx));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let mut seen = log.lock().unwrap();
            match event {
                Event::Joined { id } => seen.id = Some(id),
                Event::PeerStats { stats, .. } => {
                    seen.video_kbps = stats.video_receiving_kbps;
                    seen.video_loss = stats.video_loss_percent;
                }
                Event::OwnShare(Some(sharing)) => {
                    seen.own = Some((sharing.sent_fps, sharing.encode_ms));
                }
                Event::Watching { stats, .. } => {
                    seen.watch = Some((stats.dropped_percent, stats.decode_ms));
                }
                Event::ShareFailed(reason) => seen.problems.push(reason),
                Event::WatchFailed { reason, .. } => seen.problems.push(reason),
                _ => {}
            }
        }
    });
    Client { commands, seen }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "takes minutes"]
async fn a_share_survives_being_pushed() {
    gstreamer::init().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let config = Config {
        ice_servers: vec![],
        releases: None,
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });

    let sharer = join(&url, "sharer");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let viewer = join(&url, "viewer");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let rounds = env_or("STRESS_ROUNDS", 60);
    let dwell_ms = env_or("STRESS_DWELL_MS", 2500);
    let top = env_or("STRESS_TOP_KBPS", 50_000) as u32;
    let mut settings = ShareSettings {
        max_height: None,
        fps: env_or("STRESS_FPS", 60) as u32,
        sound: false,
        codec: Codec::H264,
        kbps: Some(top),
    };
    // A PipeWire node to share in place of the test picture, such as one
    // made with `videotestsrc ! pipewiresink mode=provide`.
    // On Windows it is the number of a monitor instead.
    let screen = std::env::var("STRESS_NODE")
        .ok()
        .and_then(|id| id.parse().ok());
    #[cfg(not(windows))]
    let screen = screen.map(|node| ScreenSource { remote: None, node });
    #[cfg(windows)]
    let screen = screen.map(ScreenSource::MonitorNumber);
    let source = match screen {
        Some(screen) => ShareSource::Screen(screen),
        None => ShareSource::TestPicture {
            width: 2560,
            height: 1440,
        },
    };
    sharer
        .commands
        .send(Command::StartShare { source, settings })
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let sharer_id = sharer.seen.lock().unwrap().id.unwrap();
    viewer
        .commands
        .send(Command::Watch {
            id: sharer_id,
            watch: true,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    // A fixed sequence that looks random, so that a failure can be repeated.
    let mut state: u64 = env_or("STRESS_SEED", 1);
    let mut next = |n: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % n
    };
    let hold = env_or("STRESS_HOLD", 0) == 1;
    if env_or("STRESS_HEVC", 0) == 1 {
        settings.codec = Codec::Hevc;
        sharer
            .commands
            .send(Command::SetShareSettings(settings))
            .unwrap();
    }
    let rates = [Some(top), Some(top), None, Some(1000), Some(top / 2)];
    let mut dead = Vec::new();
    for round in 0..rounds {
        match if hold { 3 } else { next(3) } {
            3 => {}
            0 => {
                settings.codec = match settings.codec {
                    Codec::H264 => Codec::Hevc,
                    Codec::Hevc | Codec::Av1 => Codec::H264,
                }
            }
            1 => settings.kbps = rates[next(rates.len() as u64) as usize],
            _ => {
                settings.codec = match settings.codec {
                    Codec::H264 => Codec::Hevc,
                    Codec::Hevc | Codec::Av1 => Codec::H264,
                };
                settings.kbps = rates[next(rates.len() as u64) as usize];
            }
        }
        if !hold {
            sharer
                .commands
                .send(Command::SetShareSettings(settings))
                .unwrap();
        }
        // The first part of the dwell is for the change to settle.
        tokio::time::sleep(Duration::from_millis(dwell_ms / 2)).await;
        viewer.seen.lock().unwrap().frames = 0;
        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(dwell_ms / 2)).await;
        let seen = viewer.seen.lock().unwrap();
        let fps = seen.frames as f64 / start.elapsed().as_secs_f64();
        let (sent_fps, encode_ms) = sharer.seen.lock().unwrap().own.unwrap_or_default();
        let (dropped, decode_ms) = seen.watch.unwrap_or_default();
        println!(
            "round {round}: {} at {:?} kbps -> {fps:.1} fps, {:.0} kbps arriving, {:.2}% lost; \
             sent {sent_fps:.1} fps, encode {encode_ms:.1} ms; shown after {decode_ms:.1} ms, \
             {dropped:.0}% skipped",
            settings.codec.label(),
            settings.kbps,
            seen.video_kbps.unwrap_or(0.0),
            seen.video_loss.unwrap_or(0.0),
        );
        if fps < env_or("STRESS_FPS", 60) as f64 / 2.0 {
            dead.push((round, settings, fps));
        }
    }
    let problems: Vec<String> = [&sharer, &viewer]
        .iter()
        .flat_map(|client| client.seen.lock().unwrap().problems.clone())
        .collect();
    assert!(problems.is_empty(), "{problems:?}");
    assert!(dead.is_empty(), "the picture stopped: {dead:?}");
}
