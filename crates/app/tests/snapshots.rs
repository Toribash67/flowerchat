//! Renders each screen from a fixed model and compares it with the image in
//! `tests/snapshots`. Run with `UPDATE_SNAPSHOTS=1` to accept new output.

use std::time::Duration;

use egui::Vec2;
use egui_kittest::Harness;
use flowerchat_app::model::{Entry, EntryKind, PeerRow, RoomModel};
use flowerchat_app::settings::{DeviceChoice, SendMode};
use flowerchat_app::talk::GlobalKey;
use flowerchat_app::views::{self, Env, Screen, Tab, UiState};
use flowerchat_client::audio::{AudioDevice, AudioDevices};
use flowerchat_client::session::{ConnectionState, PeerStats, Route};

const WIDE: Vec2 = Vec2::new(1280.0, 720.0);
const NARROW: Vec2 = Vec2::new(360.0, 640.0);

fn peer(id: u64, name: &str, route: Route, rtt: f64, loss: f64, jitter: f64, kbps: f64) -> PeerRow {
    let mut peer = PeerRow::new(id, name);
    peer.state = ConnectionState::Connected;
    peer.stats = PeerStats {
        route: Some(route),
        round_trip_ms: Some(rtt),
        loss_percent: Some(loss),
        jitter_ms: Some(jitter),
        receiving_kbps: Some(kbps),
        sending_kbps: Some(64.0),
    };
    peer
}

/// A room showing every state a row can be in.
fn room() -> RoomModel {
    let mut room = RoomModel::new("lounge", "chat.example.org", "robin");
    room.in_room = Duration::from_secs(42 * 60 + 17);
    room.me.mic_level = 0.42;
    room.me.sending_kbps = Some(64.0);

    let mut alex = peer(2, "alex", Route::Direct, 18.0, 0.0, 2.0, 61.0);
    alex.speaking = true;
    let mut sam = peer(3, "sam", Route::Direct, 24.0, 0.0, 3.0, 58.0);
    sam.volume = 1.4;
    let mut kim = peer(4, "kim", Route::Direct, 31.0, 0.2, 4.0, 6.0);
    kim.muted = true;
    kim.volume = 0.6;
    let jo = peer(5, "jo", Route::Relayed, 71.0, 1.4, 11.0, 60.0);
    let charlie = PeerRow::new(6, "charlie");
    room.peers = vec![alex, sam, kim, jo, charlie];

    let event = |time: &str, text: &str| Entry {
        time: time.into(),
        kind: EntryKind::Event(text.into()),
    };
    let message = |time: &str, from: &str, text: &str| Entry {
        time: time.into(),
        kind: EntryKind::Message {
            from: from.into(),
            text: text.into(),
        },
    };
    room.timeline = vec![
        event("20:58", "You joined lounge"),
        event("20:58", "alex connected"),
        event("20:58", "sam connected"),
        message(
            "21:00",
            "sam",
            "evening! is the sound better than last time?",
        ),
        message("21:01", "robin", "much better on my end, no echo at all"),
        event("21:02", "kim muted"),
        event("21:03", "jo connected"),
        message("21:04", "alex", "can everyone hear me alright?"),
        event("21:04", "charlie joined, connecting"),
    ];
    room
}

fn form() -> UiState {
    UiState {
        name: "robin".into(),
        room: "lounge".into(),
        server: "wss://chat.example.org/ws".into(),
        ..UiState::default()
    }
}

fn devices() -> AudioDevices {
    let device = |id: &str, label: &str, is_default: bool| AudioDevice {
        id: id.into(),
        label: label.into(),
        is_default,
    };
    AudioDevices {
        inputs: vec![
            device("headset-mic", "USB headset", true),
            device("onboard-mic", "Built-in audio", false),
        ],
        outputs: vec![
            device("headset", "USB headset", true),
            device("hdmi", "Monitor (HDMI)", false),
        ],
    }
}

/// An ordinary machine with an open mic.
fn env(devices: &AudioDevices) -> Env<'_> {
    Env {
        devices,
        global_key: GlobalKey::Off,
        transmitting: true,
    }
}

fn snapshot(name: &str, size: Vec2, mut draw: impl FnMut(&mut egui::Ui)) {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.0)
        .wgpu()
        .build_ui(|ui| draw(ui));
    harness.run();
    harness.snapshot(name);
}

#[test]
fn lobby_wide() {
    let mut state = form();
    let devices = devices();
    let screen = || Screen::Lobby {
        connecting: false,
        error: None,
        mic_level: 0.42,
    };
    snapshot("lobby_wide", WIDE, |ui| {
        views::draw(ui, screen(), &env(&devices), &mut state);
    });
}

#[test]
fn lobby_with_error() {
    let mut state = form();
    let devices = devices();
    snapshot("lobby_with_error", NARROW, |ui| {
        let screen = Screen::Lobby {
            connecting: false,
            error: Some("cannot reach the signaling server at wss://chat.example.org/ws"),
            mic_level: 0.0,
        };
        views::draw(ui, screen, &env(&devices), &mut state);
    });
}

#[test]
fn room_wide() {
    let room = room();
    let mut state = form();
    let devices = devices();
    snapshot("room_wide", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_wide_muted_and_deafened() {
    let mut room = room();
    room.me.muted = true;
    room.me.deafened = true;
    let mut state = form();
    let devices = devices();
    snapshot("room_wide_muted_and_deafened", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_narrow_people() {
    let room = room();
    let mut state = form();
    let devices = devices();
    snapshot("room_narrow_people", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_narrow_chat() {
    let room = room();
    let mut state = form();
    state.tab = Tab::Chat;
    let devices = devices();
    snapshot("room_narrow_chat", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_wide_push_to_talk() {
    let room = room();
    let mut state = form();
    state.settings.send = SendMode::WhileKeyHeld;
    state.settings.talk_key = Some("F8".into());
    state.settings.noise_suppression = false;
    state.settings.receive_buffer_ms = 80;
    // Chosen once, but not plugged in now.
    state.settings.microphone = Some(DeviceChoice {
        id: "webcam-mic".into(),
        label: "Webcam".into(),
    });
    let devices = devices();
    let env = Env {
        devices: &devices,
        global_key: GlobalKey::Ready {
            trigger: String::new(),
        },
        transmitting: false,
    };
    snapshot("room_wide_push_to_talk", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}

#[test]
fn room_narrow_audio() {
    let room = room();
    let mut state = form();
    state.tab = Tab::Audio;
    state.settings.send = SendMode::WhileKeyHeld;
    let devices = devices();
    let env = Env {
        devices: &devices,
        global_key: GlobalKey::Unavailable(
            "the desktop does not know this app (is se.flowerboi.flowerchat.desktop installed?)"
                .into(),
        ),
        transmitting: false,
    };
    snapshot("room_narrow_audio", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}
