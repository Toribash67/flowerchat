//! What the tests of sharing have in common: a server, and clients that
//! note down what they see.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flowerchat_client::session::{
    self, AudioSettings, Command, Event, FrameSink, SessionConfig, Sharing, VideoFrame, WatchStats,
};
use flowerchat_protocol::PeerId;
use flowerchat_server::{Config, app};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// Starts a signaling server and returns its address.
pub async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    // No STUN server: the clients find each other on local addresses.
    let config = Config {
        ice_servers: vec![],
        releases: None,
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });
    url
}

/// What a client has seen happen.
#[derive(Default)]
pub struct Seen {
    pub id: Option<PeerId>,
    /// Delays of the clicks received, in milliseconds.
    pub delays: Vec<f64>,
    /// Size of every frame received of the screens being watched.
    pub frames: Vec<(u32, u32)>,
    /// Loudness reports of the sound of the screens being watched, in dB.
    pub sound: Vec<f32>,
    /// Bitrate of the video received, in kbit/s, second by second.
    pub video_kbps: Vec<f64>,
    /// The last word on our own share and on each peer's.
    pub own_share: Option<Sharing>,
    pub peer_share: Option<Sharing>,
    /// How watching goes, as last reported.
    pub watching: Option<WatchStats>,
    pub problems: Vec<String>,
}

pub struct Client {
    commands: UnboundedSender<Command>,
    pub seen: Arc<Mutex<Seen>>,
    pub session: JoinHandle<anyhow::Result<()>>,
}

impl Client {
    pub fn id(&self) -> PeerId {
        self.seen.lock().unwrap().id.expect("the client has joined")
    }

    pub fn send(&self, command: Command) {
        self.commands.send(command).unwrap();
    }

    /// The frames that arrive in the next `seconds`: how many per second,
    /// and the size of the last.
    pub async fn frames_over(&self, seconds: u64) -> (f64, Option<(u32, u32)>) {
        self.seen.lock().unwrap().frames.clear();
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let frames = std::mem::take(&mut self.seen.lock().unwrap().frames);
        let rate = frames.len() as f64 / start.elapsed().as_secs_f64();
        (rate, frames.last().copied())
    }

    /// The loudest the sound of the watched screen gets in the next
    /// `seconds`, or `None` if none arrives at all.
    pub async fn sound_over(&self, seconds: u64) -> Option<f32> {
        self.seen.lock().unwrap().sound.clear();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let sound = std::mem::take(&mut self.seen.lock().unwrap().sound);
        sound.into_iter().reduce(f32::max)
    }

    /// The bitrate of the video that arrives over the next `seconds`, in
    /// kbit/s.
    pub async fn video_kbps_over(&self, seconds: u64) -> f64 {
        self.seen.lock().unwrap().video_kbps.clear();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let rates = std::mem::take(&mut self.seen.lock().unwrap().video_kbps);
        assert!(!rates.is_empty(), "video bitrate is reported");
        rates.iter().sum::<f64>() / rates.len() as f64
    }

    /// The median delay of the clicks that arrive in the next few seconds.
    pub async fn median_delay(&self) -> f64 {
        self.seen.lock().unwrap().delays.clear();
        tokio::time::sleep(Duration::from_secs(4)).await;
        let mut delays = std::mem::take(&mut self.seen.lock().unwrap().delays);
        assert!(delays.len() >= 2, "clicks keep arriving");
        delays.sort_by(f64::total_cmp);
        delays[delays.len() / 2]
    }
}

/// Joins the room. `loss` is the share of arriving packets the client
/// loses on purpose, and `software_video` keeps its graphics card out of
/// encoding and decoding.
pub fn join(url: &str, name: &str, loss: f64, software_video: bool) -> Client {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let frame_log = seen.clone();
    let frames: FrameSink = Arc::new(move |_, frame: VideoFrame| {
        assert_eq!(
            frame.rgba().len(),
            frame.width as usize * frame.height as usize * 4
        );
        frame_log
            .lock()
            .unwrap()
            .frames
            .push((frame.width, frame.height));
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
        software_video,
        incoming_loss: loss,
    };
    let session = tokio::spawn(session::run(config, command_rx, event_tx));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let mut seen = log.lock().unwrap();
            match event {
                Event::Joined { id } => seen.id = Some(id),
                Event::ClickDelay(ns) => seen.delays.push(ns as f64 / 1e6),
                Event::StreamLevel { db, .. } => seen.sound.push(db),
                Event::PeerStats { stats, .. } => {
                    seen.video_kbps.extend(stats.video_receiving_kbps);
                }
                Event::OwnShare(sharing) => seen.own_share = sharing,
                Event::PeerSharing { sharing, .. } => seen.peer_share = sharing,
                Event::Watching { stats, .. } => seen.watching = Some(stats),
                Event::ShareFailed(reason) => seen.problems.push(reason),
                Event::WatchFailed { reason, .. } => seen.problems.push(reason),
                _ => {}
            }
        }
    });
    Client {
        commands,
        seen,
        session,
    }
}
