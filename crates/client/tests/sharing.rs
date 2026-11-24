//! Shares a test picture from one client and watches it from another, with
//! both timing each other's clicks as in `selftest.rs`.
//!
//! Needs an NVIDIA GPU for encoding and decoding, but no screen, window or
//! audio device.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flowerchat_client::session::{
    self, AudioSettings, Command, Event, FrameSink, SessionConfig, ShareSettings, ShareSource,
    Sharing, VideoFrame,
};
use flowerchat_protocol::PeerId;
use flowerchat_server::{Config, app};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// What a client has seen happen.
#[derive(Default)]
struct Seen {
    id: Option<PeerId>,
    /// Delays of the clicks received, in milliseconds.
    delays: Vec<f64>,
    /// Size of every frame received of the screens being watched.
    frames: Vec<(u32, u32)>,
    /// The last word on our own share and on each peer's.
    own_share: Option<Sharing>,
    peer_share: Option<Sharing>,
    problems: Vec<String>,
}

struct Client {
    commands: UnboundedSender<Command>,
    seen: Arc<Mutex<Seen>>,
    session: JoinHandle<anyhow::Result<()>>,
}

impl Client {
    fn id(&self) -> PeerId {
        self.seen.lock().unwrap().id.expect("the client has joined")
    }

    fn send(&self, command: Command) {
        self.commands.send(command).unwrap();
    }

    /// The frames that arrive in the next `seconds`: how many per second,
    /// and the size of the last.
    async fn frames_over(&self, seconds: u64) -> (f64, Option<(u32, u32)>) {
        self.seen.lock().unwrap().frames.clear();
        let start = Instant::now();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let frames = std::mem::take(&mut self.seen.lock().unwrap().frames);
        let rate = frames.len() as f64 / start.elapsed().as_secs_f64();
        (rate, frames.last().copied())
    }

    /// The median delay of the clicks that arrive in the next few seconds.
    async fn median_delay(&self) -> f64 {
        self.seen.lock().unwrap().delays.clear();
        tokio::time::sleep(Duration::from_secs(4)).await;
        let mut delays = std::mem::take(&mut self.seen.lock().unwrap().delays);
        assert!(delays.len() >= 2, "clicks keep arriving");
        delays.sort_by(f64::total_cmp);
        delays[delays.len() / 2]
    }
}

/// Joins the room. `loss` is the share of arriving packets the client
/// loses on purpose.
fn join(url: &str, name: &str, loss: f64) -> Client {
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
                Event::OwnShare(sharing) => seen.own_share = sharing,
                Event::PeerSharing { sharing, .. } => seen.peer_share = sharing,
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

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_screen_reaches_whoever_watches() {
    gstreamer::init().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    // No STUN server: the two clients find each other on local addresses.
    let config = Config {
        ice_servers: vec![],
    };
    tokio::spawn(async move { axum::serve(listener, app(config)).await.unwrap() });

    let sharer = join(&url, "sharer", 0.0);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let viewer = join(&url, "viewer", 0.0);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let voice_before = viewer.median_delay().await;

    // Sharing alone sends nothing: the stream is for those who ask.
    sharer.send(Command::StartShare {
        source: ShareSource::TestPicture {
            width: 1280,
            height: 720,
        },
        settings: ShareSettings {
            max_height: None,
            fps: 60,
        },
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let announced = viewer.seen.lock().unwrap().peer_share.clone();
    let announced = announced.expect("the viewer hears of the share");
    assert_eq!((announced.width, announced.height), (1280, 720));
    assert_eq!((announced.fps, announced.kbps), (60, 3900));
    assert!(announced.watchers.is_empty());
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "nobody asked to watch");

    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rate, size) = viewer.frames_over(3).await;
    assert_eq!(size, Some((1280, 720)));
    assert!((55.0..65.0).contains(&rate), "60 fps arrive as {rate:.1}");
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id()]);
    assert!((55.0..65.0).contains(&own.sent_fps));

    // The voice is as quick as it was, in both directions.
    let voice_watching = viewer.median_delay().await;
    let voice_sharing = sharer.median_delay().await;
    for (what, delay) in [("watching", voice_watching), ("sharing", voice_sharing)] {
        assert!(
            (delay - voice_before).abs() < 15.0,
            "voice took {voice_before:.1} ms before and {delay:.1} ms while {what}"
        );
    }

    // Someone on a poor connection joins and watches too. The packets that
    // go missing are sent again in time, so no frames are lost, and the
    // first viewer notices nothing.
    let lossy = join(&url, "lossy", 0.02);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(lossy.seen.lock().unwrap().peer_share.is_some());
    lossy.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    let (lossy_frames, steady_frames) = tokio::join!(lossy.frames_over(4), viewer.frames_over(4));
    assert!(
        lossy_frames.0 > 50.0,
        "with loss, {:.1} fps",
        lossy_frames.0
    );
    assert!(
        steady_frames.0 > 55.0,
        "beside it, {:.1} fps",
        steady_frames.0
    );
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id(), lossy.id()]);
    lossy.send(Command::Leave);
    lossy.session.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert_eq!(own.watchers, vec![viewer.id()]);

    // A smaller, slower picture takes over without the viewer doing a thing.
    sharer.send(Command::SetShareSettings(ShareSettings {
        max_height: Some(360),
        fps: 30,
    }));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rate, size) = viewer.frames_over(3).await;
    assert_eq!(size, Some((640, 360)));
    assert!((27.0..33.0).contains(&rate), "30 fps arrive as {rate:.1}");
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.width, announced.height), (640, 360));
    assert_eq!(announced.fps, 30);

    // Looking away stops the stream; looking again restarts it.
    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: false,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "no longer watching");
    let own = sharer.seen.lock().unwrap().own_share.clone().unwrap();
    assert!(own.watchers.is_empty());
    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: true,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (rate, size) = viewer.frames_over(2).await;
    assert_eq!(size, Some((640, 360)));
    assert!((27.0..33.0).contains(&rate), "30 fps arrive as {rate:.1}");

    sharer.send(Command::StopShare);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(viewer.seen.lock().unwrap().peer_share, None);
    assert_eq!(sharer.seen.lock().unwrap().own_share, None);
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "the share is over");

    for client in [sharer, viewer] {
        assert_eq!(client.seen.lock().unwrap().problems, Vec::<String>::new());
        client.send(Command::Leave);
        client.session.await.unwrap().unwrap();
    }
}
