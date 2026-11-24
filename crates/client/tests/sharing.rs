//! Shares a test picture from one client and watches it from another, with
//! both timing each other's clicks as in `selftest.rs`.
//!
//! Needs an NVIDIA GPU for encoding and decoding, but no screen, window or
//! audio device.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flowerchat_client::session::{
    self, AudioSettings, Codec, Command, Event, FrameSink, SessionConfig, ShareSettings,
    ShareSource, Sharing, VideoFrame,
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
    /// Loudness reports of the sound of the screens being watched, in dB.
    sound: Vec<f32>,
    /// Bitrate of the video received, in kbit/s, second by second.
    video_kbps: Vec<f64>,
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

    /// The loudest the sound of the watched screen gets in the next
    /// `seconds`, or `None` if none arrives at all.
    async fn sound_over(&self, seconds: u64) -> Option<f32> {
        self.seen.lock().unwrap().sound.clear();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let sound = std::mem::take(&mut self.seen.lock().unwrap().sound);
        sound.into_iter().reduce(f32::max)
    }

    /// The bitrate of the video that arrives over the next `seconds`, in
    /// kbit/s.
    async fn video_kbps_over(&self, seconds: u64) -> f64 {
        self.seen.lock().unwrap().video_kbps.clear();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let rates = std::mem::take(&mut self.seen.lock().unwrap().video_kbps);
        assert!(!rates.is_empty(), "video bitrate is reported");
        rates.iter().sum::<f64>() / rates.len() as f64
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
                Event::StreamLevel { db, .. } => seen.sound.push(db),
                Event::PeerStats { stats, .. } => {
                    seen.video_kbps.extend(stats.video_receiving_kbps);
                }
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
    let full = ShareSettings {
        max_height: None,
        fps: 60,
        sound: true,
        codec: Codec::H264,
        kbps: None,
    };
    sharer.send(Command::StartShare {
        source: ShareSource::TestPicture {
            width: 1280,
            height: 720,
        },
        settings: full,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    let announced = viewer.seen.lock().unwrap().peer_share.clone();
    let announced = announced.expect("the viewer hears of the share");
    assert_eq!((announced.width, announced.height), (1280, 720));
    assert_eq!((announced.fps, announced.kbps), (60, 3900));
    assert_eq!(announced.codec, Codec::H264);
    assert!(announced.watchers.is_empty() && announced.sound);
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "nobody asked to watch");
    assert_eq!(viewer.sound_over(1).await, None, "nor to listen");

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
    // The share's sound, a test tone, comes with the picture.
    let sound = viewer.sound_over(2).await.expect("the share has sound");
    assert!(
        (-30.0..-15.0).contains(&sound),
        "the tone arrives at {sound:.0} dB"
    );

    // A viewer with less room for the picture gets its frames smaller, and
    // at full size again when it asks.
    viewer.send(Command::SetViewHeight {
        id: sharer.id(),
        max_height: 360,
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (rate, size) = viewer.frames_over(2).await;
    assert_eq!(size, Some((640, 360)));
    assert!(rate > 50.0, "scaled down, {rate:.1} fps");
    viewer.send(Command::SetViewHeight {
        id: sharer.id(),
        max_height: 0,
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(viewer.frames_over(1).await.1, Some((1280, 720)));

    // The voice is as quick as it was, in both directions.
    let voice_watching = viewer.median_delay().await;
    let voice_sharing = sharer.median_delay().await;
    for (what, delay) in [("watching", voice_watching), ("sharing", voice_sharing)] {
        assert!(
            (delay - voice_before).abs() < 15.0,
            "voice took {voice_before:.1} ms before and {delay:.1} ms while {what}"
        );
    }

    // The sharer changes codec and sets a bitrate. The viewer does nothing
    // and keeps getting every frame.
    let rate_before = viewer.video_kbps_over(3).await;
    assert!(
        (3000.0..5000.0).contains(&rate_before),
        "3.9 Mbit/s arrive as {rate_before:.0} kbit/s"
    );
    let hevc = ShareSettings {
        codec: Codec::Hevc,
        ..full
    };
    sharer.send(Command::SetShareSettings(hevc));
    let (rate, size) = viewer.frames_over(3).await;
    assert_eq!(size, Some((1280, 720)));
    assert!(rate > 50.0, "across the change, {rate:.1} fps");
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::Hevc, 2900));
    sharer.send(Command::SetShareSettings(ShareSettings {
        kbps: Some(1000),
        ..hevc
    }));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (frames, rate_set) = tokio::join!(viewer.frames_over(3), viewer.video_kbps_over(3));
    assert!(
        (55.0..65.0).contains(&frames.0),
        "in HEVC, {:.1} fps",
        frames.0
    );
    assert!(
        (700.0..1300.0).contains(&rate_set),
        "1 Mbit/s arrives as {rate_set:.0} kbit/s"
    );
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::Hevc, 1000));
    // Looking away and back starts a new decoder, which needs the stream
    // to pick up with everything a decoder starts from.
    for watch in [false, true] {
        viewer.send(Command::Watch {
            id: sharer.id(),
            watch,
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let (rate, _) = viewer.frames_over(2).await;
    assert!(
        (55.0..65.0).contains(&rate),
        "watching HEVC again, {rate:.1} fps"
    );
    sharer.send(Command::SetShareSettings(full));
    let (rate, _) = viewer.frames_over(3).await;
    assert!(rate > 50.0, "across the change back, {rate:.1} fps");
    let (frames, rate_back) = tokio::join!(viewer.frames_over(3), viewer.video_kbps_over(3));
    assert!(
        (55.0..65.0).contains(&frames.0),
        "back in H.264, {:.1} fps",
        frames.0
    );
    assert!(
        (3000.0..5000.0).contains(&rate_back),
        "3.9 Mbit/s arrive again as {rate_back:.0} kbit/s"
    );
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.codec, announced.kbps), (Codec::H264, 3900));

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
    // The sound is turned off with it, and back on afterwards.
    let quiet = ShareSettings {
        max_height: Some(360),
        fps: 30,
        sound: false,
        ..full
    };
    sharer.send(Command::SetShareSettings(quiet));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (frames, sound) = tokio::join!(viewer.frames_over(3), viewer.sound_over(3));
    let (rate, size) = frames;
    assert_eq!(size, Some((640, 360)));
    assert!((27.0..33.0).contains(&rate), "30 fps arrive as {rate:.1}");
    assert_eq!(sound, None, "the picture goes on without sound");
    let announced = viewer.seen.lock().unwrap().peer_share.clone().unwrap();
    assert_eq!((announced.width, announced.height), (640, 360));
    assert_eq!((announced.fps, announced.sound), (30, false));
    sharer.send(Command::SetShareSettings(ShareSettings {
        sound: true,
        ..quiet
    }));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(viewer.sound_over(2).await.is_some(), "the sound is back");

    // Looking away stops the stream; looking again restarts it.
    viewer.send(Command::Watch {
        id: sharer.id(),
        watch: false,
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(viewer.frames_over(1).await.0, 0.0, "no longer watching");
    assert_eq!(viewer.sound_over(1).await, None, "nor listening");
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
