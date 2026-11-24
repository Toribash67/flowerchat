//! Renders each screen from a fixed model and compares it with the image in
//! `tests/snapshots`. Run with `UPDATE_SNAPSHOTS=1` to accept new output.

use std::time::Duration;

use egui::Vec2;
use egui_kittest::Harness;
use flowerchat_app::model::{AudioSummary, Entry, EntryKind, PeerRow, RoomModel};
use flowerchat_app::views::{self, Screen, Tab, UiState};
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
    let audio = AudioSummary {
        microphone: "System default".into(),
        speakers: "System default".into(),
        processing: true,
        receive_buffer_ms: 40,
    };
    let mut room = RoomModel::new("lounge", "chat.example.org", "robin", audio);
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
    let screen = || Screen::Lobby {
        connecting: false,
        error: None,
    };
    snapshot("lobby_wide", WIDE, |ui| {
        views::draw(ui, screen(), &mut state);
    });
}

#[test]
fn lobby_with_error() {
    let mut state = form();
    snapshot("lobby_with_error", NARROW, |ui| {
        let screen = Screen::Lobby {
            connecting: false,
            error: Some("cannot reach the signaling server at wss://chat.example.org/ws"),
        };
        views::draw(ui, screen, &mut state);
    });
}

#[test]
fn room_wide() {
    let room = room();
    let mut state = form();
    snapshot("room_wide", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &mut state);
    });
}

#[test]
fn room_wide_muted_and_deafened() {
    let mut room = room();
    room.me.muted = true;
    room.me.deafened = true;
    let mut state = form();
    snapshot("room_wide_muted_and_deafened", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &mut state);
    });
}

#[test]
fn room_narrow_people() {
    let room = room();
    let mut state = form();
    snapshot("room_narrow_people", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &mut state);
    });
}

#[test]
fn room_narrow_chat() {
    let room = room();
    let mut state = form();
    state.tab = Tab::Chat;
    snapshot("room_narrow_chat", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &mut state);
    });
}
