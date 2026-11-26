//! Drives the whole app, without a window, through a real call: joins a
//! room, changes every audio setting from the interface and checks what
//! another participant and the sound devices get to hear.
//!
//! Ignored by default, because it adds three virtual sound devices to the
//! system with `pactl` for as long as it runs. It neither plays on nor
//! listens to any real device. Run it with
//! `cargo test -p flowerchat-app -- --ignored`.

use std::process::Command as Process;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use flowerchat_client::audio::{AudioSettings, MicCheck};
use flowerchat_client::session::{
    self, Codec, Command, Event, FrameSink, SessionConfig, ShareSettings, ShareSource, Sharing,
};
use flowerchat_server::Config;
use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::mpsc::unbounded_channel;

use super::{App, Args};

const ROOM: &str = "lounge";
/// Carries a steady tone, and stands in for the microphone.
const MICROPHONE: &str = "fc_e2e_mic";
const SPEAKERS: [&str; 2] = ["fc_e2e_one", "fc_e2e_two"];
/// Quieter than this counts as silence.
const SILENT_DB: f32 = -80.0;
/// Louder than this counts as sound.
const AUDIBLE_DB: f32 = -30.0;

/// Loudness reports, in dB below full scale.
type Levels = Arc<Mutex<Vec<f32>>>;

/// The virtual devices, for as long as this lives.
struct VirtualDevices {
    modules: Vec<String>,
    tone: gst::Element,
}

impl VirtualDevices {
    fn create() -> Self {
        let modules = [MICROPHONE, SPEAKERS[0], SPEAKERS[1]]
            .map(|name| {
                let output = Process::new("pactl")
                    .args(["load-module", "module-null-sink"])
                    .arg(format!("sink_name={name}"))
                    .arg(format!("sink_properties=device.description={name}"))
                    .output()
                    .expect("pactl is needed to create virtual sound devices");
                assert!(output.status.success(), "pactl could not create {name}");
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            })
            .to_vec();
        let tone = gst::parse::launch(&format!(
            "audiotestsrc is-live=true wave=sine freq=440 volume=0.3 \
             ! audioconvert ! pulsesink device={MICROPHONE}"
        ))
        .unwrap();
        tone.set_state(gst::State::Playing).unwrap();
        // The devices take a moment to show up everywhere.
        std::thread::sleep(Duration::from_millis(500));
        Self { modules, tone }
    }
}

impl Drop for VirtualDevices {
    fn drop(&mut self) {
        let _ = self.tone.set_state(gst::State::Null);
        for module in &self.modules {
            let _ = Process::new("pactl")
                .args(["unload-module", module])
                .status();
        }
    }
}

/// Draws frames for a while, as an open window would.
fn run(harness: &mut Harness<'_, App>, seconds: f64) {
    // Twenty frames a second by the clock, drawing included. How long
    // drawing takes depends on what else the machine is doing, and what
    // the tests count over this time, such as the frames of a shared
    // screen, does not wait for it. The number of frames drawn stays the
    // same either way: a menu needs a few to open before it can be clicked.
    let started = std::time::Instant::now();
    for frame in 1..=(seconds / 0.05) as u32 {
        harness.step();
        let due = started + Duration::from_millis(50) * frame;
        std::thread::sleep(due.saturating_duration_since(std::time::Instant::now()));
    }
}

fn loudest(levels: &Levels) -> f32 {
    let mut levels = levels.lock().unwrap();
    let peak = levels.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    levels.clear();
    peak
}

/// The loudest that `levels` reports from now on, for `seconds`.
fn hear(harness: &mut Harness<'_, App>, levels: &Levels, seconds: f64) -> f32 {
    loudest(levels);
    run(harness, seconds);
    loudest(levels)
}

/// The same, once a change that was just made has had time to take effect.
fn hear_after_change(harness: &mut Harness<'_, App>, levels: &Levels) -> f32 {
    run(harness, 0.8);
    hear(harness, levels, 1.5)
}

/// Listens to what is played on a virtual speaker.
fn listen_to(speaker: &str) -> (MicCheck, Levels) {
    let levels = Levels::default();
    let sink = levels.clone();
    let settings = AudioSettings {
        input: Some(format!("{speaker}.monitor")),
        ..AudioSettings::default()
    }
    .unprocessed();
    let check = MicCheck::start(&settings, move |db| sink.lock().unwrap().push(db)).unwrap();
    (check, levels)
}

#[test]
#[ignore = "adds virtual sound devices to the system while it runs"]
fn audio_settings_work_through_the_interface() {
    gst::init().unwrap();
    let _devices = VirtualDevices::create();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let config = Config {
            ice_servers: vec![],
            releases: None,
            invites: None,
            rooms: vec![],
        };
        tokio::spawn(async move {
            axum::serve(listener, flowerchat_server::app(config))
                .await
                .unwrap();
        });
        url
    });

    // Someone else in the room, who sends a click every second and reports
    // how loud we are.
    let (other, other_commands) = unbounded_channel();
    let (other_events, mut events) = unbounded_channel();
    let config = SessionConfig {
        server: url.clone(),
        room: ROOM.into(),
        name: "other".into(),
        invite: None,
        audio: AudioSettings::default(),
        transmitting: true,
        test_tone: true,
        measure: true,
        frames: None,
        software_video: false,
        incoming_loss: 0.0,
    };
    let other_session = runtime.spawn(session::run(config, other_commands, other_events));
    let us: Levels = Levels::default();
    let sink = us.clone();
    runtime.spawn(async move {
        while let Some(event) = events.recv().await {
            if let Event::PeerLevel { db, .. } = event {
                sink.lock().unwrap().push(db);
            }
        }
    });
    let (_first, first) = listen_to(SPEAKERS[0]);
    let (_second, second) = listen_to(SPEAKERS[1]);

    let microphone = format!("{MICROPHONE}.monitor");
    let args = Args::parse_from([
        "flowerchat",
        "--name",
        "robin",
        "--room",
        ROOM,
        "--server",
        &url,
        "--input",
        &microphone,
        "--output",
        SPEAKERS[0],
    ]);
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 720.0))
        .build_eframe(|cc| App::new(cc, args).unwrap());
    let button = Role::Button;

    // The join screen already listens to the microphone.
    run(&mut harness, 1.5);
    let level = f32::from_bits(
        harness
            .state()
            .lobby_level
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    assert!(level > 0.5, "the join screen shows the microphone's level");

    harness.get_by_role_and_label(button, ROOM).click();
    run(&mut harness, 5.0);
    assert!(harness.query_by_label("In room").is_some(), "joined");
    assert!(harness.state().mic_check.is_none());
    let processed = hear_after_change(&mut harness, &us);
    assert!(processed > AUDIBLE_DB, "the other side hears us");
    assert!(hear(&mut harness, &first, 2.5) > AUDIBLE_DB, "we play them");
    assert!(hear(&mut harness, &second, 2.5) < SILENT_DB);

    // Processing: without it, the tone arrives at the level it has.
    harness.get_by_label("Noise suppression").click();
    harness.step();
    harness.get_by_label("Automatic volume").click();
    harness.step();
    let unprocessed = hear_after_change(&mut harness, &us);
    assert!(
        unprocessed > processed + 2.0,
        "turning processing off changes what is sent"
    );

    // Push-to-talk, with a key chosen in the window.
    harness.get_by_label("While a key is held").click();
    harness.step();
    assert!(hear_after_change(&mut harness, &us) < SILENT_DB);
    assert!(harness.query_by_label("Hold key to talk").is_some());
    harness.get_by_role_and_label(button, "Set key").click();
    harness.step();
    harness.key_press(egui::Key::F8);
    run(&mut harness, 0.1);
    assert_eq!(harness.state().ui.settings.talk_key.as_deref(), Some("F8"));
    assert!(
        hear_after_change(&mut harness, &us) < SILENT_DB,
        "choosing the key does not send anything"
    );
    harness.key_down(egui::Key::F8);
    harness.step();
    assert!(hear_after_change(&mut harness, &us) > AUDIBLE_DB);
    assert!(harness.query_by_label("Key held, sending").is_some());
    harness.key_up(egui::Key::F8);
    harness.step();
    assert!(hear_after_change(&mut harness, &us) < SILENT_DB);

    // The key does not count while it is being typed into the chat.
    harness.get_by_role(Role::TextInput).focus();
    harness.step();
    harness.key_down(egui::Key::F8);
    harness.step();
    assert!(hear_after_change(&mut harness, &us) < SILENT_DB);
    harness.key_up(egui::Key::F8);
    harness.get_by_label("Always (open mic)").click();
    harness.step();
    assert!(hear_after_change(&mut harness, &us) > AUDIBLE_DB);

    // Other speakers. The pickers further left are for sharing a screen.
    let pickers: Vec<_> = harness
        .get_all_by_role(Role::ComboBox)
        .filter(|picker| picker.rect().left() > 240.0)
        .collect();
    assert_eq!(pickers.len(), 2, "one for the microphone, one for speakers");
    pickers[1].click();
    drop(pickers);
    run(&mut harness, 0.1);
    harness.get_by_label(SPEAKERS[1]).click();
    run(&mut harness, 1.0);
    assert!(hear(&mut harness, &second, 2.5) > AUDIBLE_DB, "moved over");
    assert!(hear(&mut harness, &first, 2.5) < SILENT_DB);
    assert!(
        hear(&mut harness, &us, 1.5) > AUDIBLE_DB,
        "we are still heard after the switch"
    );

    // Deafened, nothing of the call is played, but the test sound is.
    harness.get_by_label_contains("Deafen").click();
    harness.step();
    assert!(hear_after_change(&mut harness, &second) < SILENT_DB);
    harness.get_by_role_and_label(button, "Test").click();
    assert!(hear(&mut harness, &second, 1.5) > AUDIBLE_DB);
    harness.get_by_label_contains("Undeafen").click();
    harness.step();

    // The receive buffer, from the keyboard. Its slider is the lowest one.
    let sliders: Vec<_> = harness.get_all_by_role(Role::Slider).collect();
    sliders
        .iter()
        .max_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .unwrap()
        .focus();
    drop(sliders);
    run(&mut harness, 0.1);
    harness.key_press(egui::Key::ArrowRight);
    run(&mut harness, 0.1);
    let audio = &harness.state().active.as_ref().unwrap().audio;
    assert_eq!(audio.jitter_ms, 41, "the session was given the new buffer");

    // Leaving, and joining again without restarting.
    harness.get_by_role_and_label(button, "Leave").click();
    run(&mut harness, 2.0);
    assert!(harness.state().active.is_none(), "left");
    assert_eq!(harness.state().error, None);
    assert!(harness.state().mic_check.is_some());
    harness.get_by_role_and_label(button, ROOM).click();
    run(&mut harness, 5.0);
    assert!(harness.query_by_label("In room").is_some(), "joined again");
    assert!(hear_after_change(&mut harness, &us) > AUDIBLE_DB);

    harness.get_by_role_and_label(button, "Leave").click();
    run(&mut harness, 1.5);
    assert_eq!(harness.state().error, None);
    let _ = other.send(Command::Leave);
    runtime.block_on(other_session).unwrap().unwrap();
}

/// Starts a signaling server on this machine and returns its address.
fn serve(runtime: &tokio::runtime::Runtime) -> String {
    serve_rooms(runtime, &[])
}

/// The same, with rooms that are there before anyone is in them.
fn serve_rooms(runtime: &tokio::runtime::Runtime, rooms: &[&str]) -> String {
    let rooms: Vec<String> = rooms.iter().map(|room| (*room).to_owned()).collect();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let config = Config {
            ice_servers: vec![],
            releases: None,
            invites: None,
            rooms,
        };
        tokio::spawn(async move {
            axum::serve(listener, flowerchat_server::app(config))
                .await
                .unwrap();
        });
        url
    })
}

/// The room the app is in, or on its way into.
fn room_of(harness: &Harness<'_, App>) -> Option<String> {
    let active = harness.state().active.as_ref()?;
    Some(active.shared.lock().unwrap().room.room.clone())
}

/// The server's rooms are listed, joined with a click, and left for
/// another with a click on that one.
#[test]
fn rooms_are_joined_and_changed_from_the_list() {
    gst::init().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = serve_rooms(&runtime, &["lounge", "games"]);
    let args = Args::parse_from([
        "flowerchat",
        "--name",
        "robin",
        "--server",
        &url,
        "--test-tone",
        "--measure",
    ]);
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 720.0))
        .build_eframe(|cc| App::new(cc, args).unwrap());
    let button = Role::Button;
    run(&mut harness, 1.0);
    assert!(harness.query_by_label("Not in a room").is_some());
    shot(&mut harness, "rooms-listed");

    harness.get_by_role_and_label(button, "games").click();
    run(&mut harness, 4.0);
    assert!(harness.query_by_label("In room").is_some(), "joined");
    assert_eq!(room_of(&harness).as_deref(), Some("games"));
    shot(&mut harness, "rooms-in-games");

    // On the way there the window stays laid out as a room. Drawn as often
    // as the machine manages, so that no frame of the way is missed.
    harness.get_by_role_and_label(button, "lounge").click();
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        harness.step();
        assert!(
            harness.query_by_label("Not in a room").is_none(),
            "the join screen showed between the rooms"
        );
        assert!(harness.query_by_label("In room").is_some());
    }
    assert_eq!(harness.state().error, None);
    assert!(
        harness.query_by_label("In room").is_some(),
        "joined the other"
    );
    assert_eq!(room_of(&harness).as_deref(), Some("lounge"));
    shot(&mut harness, "rooms-in-lounge");

    harness.get_by_role_and_label(button, "Leave").click();
    run(&mut harness, 1.5);
    assert!(harness.state().active.is_none(), "left");
    assert_eq!(harness.state().error, None);

    // Closing the window while in a room leaves the room first.
    harness.get_by_role_and_label(button, "games").click();
    run(&mut harness, 3.0);
    assert_eq!(room_of(&harness).as_deref(), Some("games"));
    eframe::App::on_exit(harness.state_mut());
    assert!(harness.state().active.is_none());
}

/// Saves what the window shows, for a person to look at, where
/// `FLOWERCHAT_E2E_SHOTS` says. Does nothing without it.
fn shot(harness: &mut Harness<'_, App>, name: &str) {
    if let Some(folder) = std::env::var_os("FLOWERCHAT_E2E_SHOTS") {
        let path = std::path::Path::new(&folder).join(format!("{name}.png"));
        harness.render().unwrap().save(path).unwrap();
    }
}

/// Shares a screen from the app and watches one in it, through the buttons
/// a user would press, with a test picture standing in for both screens.
/// Uses the GPU for encoding and decoding, but no screen and no sound device.
#[test]
fn screens_are_shared_and_watched_through_the_interface() {
    gst::init().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = serve(&runtime);

    // Someone else in the room, who watches what we share and then shares
    // something too.
    #[derive(Default)]
    struct Seen {
        frames: Vec<(u32, u32)>,
        /// How many loudness reports of our share's sound have come in.
        sound: usize,
        sharer: Option<u64>,
        shares: usize,
        /// The last we heard of what is shared.
        share: Option<Sharing>,
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let frame_log = seen.clone();
    let frames: FrameSink = Arc::new(move |_, frame| {
        frame_log
            .lock()
            .unwrap()
            .frames
            .push((frame.width, frame.height));
    });
    let (other, other_commands) = unbounded_channel();
    let (other_events, mut events) = unbounded_channel();
    let config = SessionConfig {
        server: url.clone(),
        room: ROOM.into(),
        name: "other".into(),
        invite: None,
        audio: AudioSettings::default(),
        transmitting: true,
        test_tone: true,
        measure: true,
        frames: Some(frames),
        software_video: false,
        incoming_loss: 0.0,
    };
    let other_session = runtime.spawn(session::run(config, other_commands, other_events));
    let log = seen.clone();
    runtime.spawn(async move {
        while let Some(event) = events.recv().await {
            let mut seen = log.lock().unwrap();
            match event {
                Event::PeerSharing { id, sharing } => {
                    seen.sharer = sharing.is_some().then_some(id);
                    seen.shares += usize::from(sharing.is_some());
                    seen.share = sharing;
                }
                Event::StreamLevel { .. } => seen.sound += 1,
                _ => {}
            }
        }
    });

    let args = Args::parse_from([
        "flowerchat",
        "--name",
        "robin",
        "--room",
        ROOM,
        "--server",
        &url,
        "--test-tone",
        "--measure",
        "--share-test",
        "1920x1080",
        "--share-height",
        "0",
    ]);
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 720.0))
        .build_eframe(|cc| App::new(cc, args).unwrap());
    let button = Role::Button;
    // Long enough for the server to have said which rooms it has.
    run(&mut harness, 1.0);
    harness.get_by_role_and_label(button, ROOM).click();
    run(&mut harness, 5.0);
    assert!(harness.query_by_label("In room").is_some(), "joined");

    // We share, and the other one watches.
    harness.get_by_label_contains("Share screen").click();
    run(&mut harness, 3.0);
    assert!(harness.query_by_label_contains("Stop sharing").is_some());
    assert!(harness.query_by_label("1920 × 1080 · 60 fps").is_some());
    assert!(harness.query_by_label("Nobody yet").is_some());
    let us = seen
        .lock()
        .unwrap()
        .sharer
        .expect("the other one hears of it");
    other
        .send(Command::Watch {
            id: us,
            watch: true,
        })
        .unwrap();
    run(&mut harness, 3.0);
    assert!(
        harness.query_by_label("Nobody yet").is_none(),
        "we see who watches"
    );
    assert!(
        harness
            .query_by_label_contains("other started watching your screen")
            .is_some()
    );
    {
        let seen = seen.lock().unwrap();
        assert!(seen.frames.len() > 60, "our screen arrives over there");
        assert_eq!(seen.frames.last(), Some(&(1920, 1080)));
        assert!(seen.sound > 0, "and its sound with it");
    }

    // Without sound, and with it again.
    harness.get_by_label("With sound").click();
    run(&mut harness, 1.5);
    seen.lock().unwrap().sound = 0;
    run(&mut harness, 1.5);
    assert_eq!(seen.lock().unwrap().sound, 0, "the picture goes on alone");
    harness.get_by_label("With sound").click();
    run(&mut harness, 2.0);
    assert!(seen.lock().unwrap().sound > 0, "the sound is back");

    // A smaller picture and fewer frames, chosen while sharing. The
    // pickers for the share are the four on the left, in two rows; the
    // others are for sound.
    let pick = |harness: &mut Harness<'_, App>, picker: usize, choice: &str| {
        let mut pickers: Vec<_> = harness
            .get_all_by_role(Role::ComboBox)
            .filter(|picker| picker.rect().left() < 240.0)
            .collect();
        pickers.sort_by(|a, b| {
            let place = |rect: egui::Rect| (rect.top().round() as i32, rect.left().round() as i32);
            place(a.rect()).cmp(&place(b.rect()))
        });
        assert_eq!(pickers.len(), 4, "size and rate, bitrate and codec");
        pickers[picker].click();
        drop(pickers);
        run(harness, 0.1);
        harness.get_by_label(choice).click();
        run(harness, 0.5);
    };
    pick(&mut harness, 0, "Up to 720p");
    pick(&mut harness, 1, "30 fps");
    run(&mut harness, 2.0);
    assert!(harness.query_by_label("1280 × 720 · 30 fps").is_some());
    seen.lock().unwrap().frames.clear();
    run(&mut harness, 2.0);
    {
        let seen = seen.lock().unwrap();
        assert!(seen.frames.iter().all(|size| *size == (1280, 720)));
        assert!(
            (50..70).contains(&seen.frames.len()),
            "{} frames in two seconds at 30 a second",
            seen.frames.len()
        );
    }
    shot(&mut harness, "sharing");

    // Another codec and a bitrate of our choosing, also while sharing. The
    // other one keeps watching through both.
    assert!(
        harness
            .query_by_label_contains("H.264 · 2.7 Mbps")
            .is_some()
    );
    pick(&mut harness, 3, "HEVC · NVENC (hardware)");
    pick(&mut harness, 2, "2 Mbps");
    run(&mut harness, 2.0);
    assert!(harness.query_by_label_contains("HEVC · 2.0 Mbps").is_some());
    seen.lock().unwrap().frames.clear();
    run(&mut harness, 2.0);
    {
        let seen = seen.lock().unwrap();
        let share = seen.share.as_ref().expect("we are still sharing");
        assert_eq!((share.codec, share.kbps), (Codec::Hevc, 2000));
        assert!(
            (50..70).contains(&seen.frames.len()),
            "{} frames in two seconds of HEVC",
            seen.frames.len()
        );
    }
    shot(&mut harness, "sharing-hevc");
    pick(&mut harness, 2, "Automatic bitrate");
    run(&mut harness, 2.0);
    assert!(harness.query_by_label_contains("HEVC · 2.1 Mbps").is_some());

    // The other one shares, and we watch.
    other
        .send(Command::StartShare {
            source: ShareSource::TestPicture {
                width: 1920,
                height: 1080,
            },
            settings: ShareSettings {
                max_height: None,
                fps: 30,
                sound: true,
                codec: Codec::Hevc,
                kbps: Some(4000),
            },
        })
        .unwrap();
    run(&mut harness, 3.0);
    assert!(
        harness
            .query_by_label_contains("1 sharing a screen")
            .is_some()
    );
    harness.get_by_label("Watch other's screen").click();
    run(&mut harness, 4.0);
    assert!(harness.query_by_label("other's screen").is_some());
    assert!(
        harness.query_by_label("1920 × 1080").is_some(),
        "frames arrive"
    );
    assert!(harness.query_by_label("1 watching: you").is_some());
    assert!(
        harness.query_by_label("Opus, stereo").is_some(),
        "its sound has its own controls"
    );
    // The frames come at about the size they are drawn at, and not at the
    // size they are sent at, which would be wasted on this window.
    let picture = harness
        .state()
        .picture
        .as_ref()
        .expect("a picture is shown");
    assert_eq!(picture.size(), [960, 540]);
    shot(&mut harness, "watching");

    harness.get_by_label_contains("Maximize").click();
    run(&mut harness, 1.5);
    assert!(
        harness
            // The frame rate shown is measured, and may read a frame off.
            .query_by_label_contains("1920 × 1080 · ")
            .is_some()
    );
    let picture = harness.state().picture.as_ref().unwrap();
    assert_eq!(
        picture.size(),
        [1280, 720],
        "a larger picture, larger frames"
    );
    shot(&mut harness, "maximized");
    harness.get_by_label_contains("Restore").click();
    run(&mut harness, 0.5);

    // Looking away, and looking again.
    harness
        .get_by_role_and_label(button, "Stop watching")
        .click();
    run(&mut harness, 1.5);
    assert!(harness.query_by_label("other's screen").is_none());
    assert!(harness.state().picture.is_none());
    harness.get_by_label("Watch other's screen").click();
    run(&mut harness, 3.0);
    assert!(
        harness.query_by_label("1920 × 1080").is_some(),
        "frames again"
    );

    // The share ending closes what we had open of it.
    other.send(Command::StopShare).unwrap();
    run(&mut harness, 2.0);
    assert!(harness.query_by_label("other's screen").is_none());
    assert!(
        harness
            .query_by_label_contains("other stopped sharing")
            .is_some()
    );

    // We stop sharing, which the other one hears about.
    harness.get_by_label_contains("Stop sharing").click();
    run(&mut harness, 2.0);
    assert!(harness.query_by_label("Not sharing").is_some());
    assert_eq!(seen.lock().unwrap().sharer, None);
    let before = seen.lock().unwrap().frames.len();
    run(&mut harness, 1.0);
    assert_eq!(
        seen.lock().unwrap().frames.len(),
        before,
        "nothing more arrives"
    );

    harness.get_by_role_and_label(button, "Leave").click();
    run(&mut harness, 1.5);
    assert_eq!(harness.state().error, None);
    let _ = other.send(Command::Leave);
    runtime.block_on(other_session).unwrap().unwrap();
}

/// On Windows, the window itself asks what to share, and then shares the
/// real screen. Needs a monitor.
#[cfg(windows)]
#[test]
fn what_to_share_is_chosen_in_the_window() {
    gst::init().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let config = Config {
            ice_servers: vec![],
            releases: None,
            invites: None,
            rooms: vec![],
        };
        tokio::spawn(async move {
            axum::serve(listener, flowerchat_server::app(config))
                .await
                .unwrap();
        });
        url
    });
    let args = Args::parse_from([
        "flowerchat",
        "--name",
        "robin",
        "--room",
        ROOM,
        "--server",
        &url,
        "--test-tone",
        "--measure",
        "--share-height",
        "0",
    ]);
    let mut harness = Harness::builder()
        .with_size(egui::Vec2::new(1280.0, 720.0))
        .build_eframe(|cc| App::new(cc, args).unwrap());
    let button = Role::Button;
    run(&mut harness, 0.5);
    harness.get_by_role_and_label(button, "Join").click();
    run(&mut harness, 5.0);
    assert!(harness.query_by_label("In room").is_some(), "joined");

    // Asked, and answered with nothing.
    harness.get_by_label_contains("Share screen").click();
    run(&mut harness, 0.5);
    assert!(harness.query_by_label("Share what?").is_some());
    assert!(
        harness
            .query_by_label_contains("Choosing a screen")
            .is_some()
    );
    shot(&mut harness, "choosing");
    harness.get_by_role_and_label(button, "Cancel").click();
    run(&mut harness, 0.5);
    assert!(harness.query_by_label("Share what?").is_none());
    assert!(harness.query_by_label_contains("Share screen").is_some());

    // Asked again, and answered with the first screen. The list is made
    // anew every second, and the choice is still there after that.
    harness.get_by_label_contains("Share screen").click();
    run(&mut harness, 1.5);
    harness.get_by_role_and_label(button, "Screen 1").click();
    run(&mut harness, 4.0);
    assert!(harness.query_by_label("Share what?").is_none());
    assert!(harness.query_by_label_contains("Stop sharing").is_some());
    assert!(
        harness.query_by_label_contains("· 60 fps").is_some(),
        "the screen is being sent"
    );
    harness.get_by_label_contains("Stop sharing").click();
    run(&mut harness, 1.0);
    assert!(harness.query_by_label_contains("Share screen").is_some());
    assert_eq!(harness.state().error, None);
}
