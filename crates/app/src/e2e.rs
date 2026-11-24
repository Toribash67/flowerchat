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
use flowerchat_client::session::{self, Command, Event, SessionConfig};
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
    for _ in 0..(seconds / 0.05) as usize {
        harness.step();
        std::thread::sleep(Duration::from_millis(50));
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
        audio: AudioSettings::default(),
        transmitting: true,
        test_tone: true,
        measure: true,
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

    harness.get_by_role_and_label(button, "Join").click();
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

    // Other speakers.
    let pickers: Vec<_> = harness.get_all_by_role(Role::ComboBox).collect();
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
    harness.get_by_role_and_label(button, "Join").click();
    run(&mut harness, 5.0);
    assert!(harness.query_by_label("In room").is_some(), "joined again");
    assert!(hear_after_change(&mut harness, &us) > AUDIBLE_DB);

    harness.get_by_role_and_label(button, "Leave").click();
    run(&mut harness, 1.5);
    assert_eq!(harness.state().error, None);
    let _ = other.send(Command::Leave);
    runtime.block_on(other_session).unwrap().unwrap();
}
