//! Renders each screen from a fixed model and compares it with the image in
//! `tests/snapshots`. Run with `UPDATE_SNAPSHOTS=1` to accept new output.

use std::time::Duration;

use egui::Vec2;
use egui_kittest::Harness;
use flowerchat_app::model::{Entry, EntryKind, OwnShare, PeerRow, RoomModel, Watching};
use flowerchat_app::screen::{Choice, Choices};
use flowerchat_app::settings::{DeviceChoice, SendMode};
use flowerchat_app::talk::GlobalKey;
use flowerchat_app::views::{self, Env, Picture, Screen, Tab, UiState};
use flowerchat_client::audio::{AudioDevice, AudioDevices};
use flowerchat_client::session::{Codec, ConnectionState, PeerStats, Route, Sharing, WatchStats};

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
        ..PeerStats::default()
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
        picture: None,
        codecs: &Codec::ALL,
    }
}

fn sharing(watchers: &[u64]) -> Sharing {
    Sharing {
        width: 1920,
        height: 1080,
        source_width: 2560,
        source_height: 1440,
        fps: 60,
        codec: Codec::H264,
        kbps: 8700,
        sent_fps: 60.0,
        encode_ms: 4.0,
        sound: true,
        watchers: watchers.to_vec(),
    }
}

/// The room with alex sharing a screen that sam and jo watch.
fn room_with_share() -> RoomModel {
    let mut room = room();
    room.peers[0].sharing = Some(sharing(&[3, 5]));
    room.peers[0].stats.video_receiving_kbps = Some(8437.0);
    room.peers[0].stats.video_loss_percent = Some(0.0);
    room.peers[0].stats.sound_receiving_kbps = Some(161.0);
    room.peers[0].stream_volume = 0.8;
    room.timeline.push(Entry {
        time: "21:04".into(),
        kind: EntryKind::Event("alex started sharing a screen".into()),
    });
    room
}

/// The same with us watching too.
fn room_watching() -> RoomModel {
    let mut room = room_with_share();
    room.watch(2, "21:05");
    room.watching = Some(Watching {
        id: 2,
        stats: WatchStats {
            width: 1920,
            height: 1080,
            fps: 59.8,
            dropped_percent: 0.1,
            decode_ms: 3.2,
        },
    });
    room.timeline.push(Entry {
        time: "21:06".into(),
        kind: EntryKind::Message {
            from: "jo".into(),
            text: "looks sharp, text is readable".into(),
        },
    });
    room
}

/// Something that reads as a desktop from a distance: a wallpaper, two
/// windows and a bar along the bottom.
fn fake_screen() -> egui::ColorImage {
    let (width, height) = (960, 540);
    let mut image = egui::ColorImage::filled([width, height], egui::Color32::from_rgb(38, 52, 74));
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, color: [u8; 3]| {
        for y in y0..y1 {
            for x in x0..x1 {
                image.pixels[y * width + x] = egui::Color32::from_rgb(color[0], color[1], color[2]);
            }
        }
    };
    fill(60, 50, 560, 400, [236, 236, 232]);
    fill(60, 50, 560, 78, [70, 78, 92]);
    fill(90, 110, 430, 122, [150, 152, 156]);
    fill(90, 140, 500, 152, [150, 152, 156]);
    fill(90, 170, 380, 182, [150, 152, 156]);
    fill(600, 120, 900, 460, [24, 26, 30]);
    fill(600, 120, 900, 148, [70, 78, 92]);
    fill(620, 170, 800, 180, [76, 195, 138]);
    fill(620, 196, 860, 206, [110, 116, 126]);
    fill(0, 510, 960, 540, [58, 66, 80]);
    image
}

/// Draws a view that shows a shared screen, with [`fake_screen`] as the
/// picture.
fn snapshot_with_picture(name: &str, size: Vec2, mut draw: impl FnMut(&mut egui::Ui, Picture)) {
    let mut texture = None;
    snapshot(name, size, |ui| {
        let texture = texture.get_or_insert_with(|| {
            ui.ctx()
                .load_texture("screen", fake_screen(), egui::TextureOptions::LINEAR)
        });
        let picture = Picture {
            texture: texture.id(),
            size: texture.size_vec2(),
        };
        draw(ui, picture);
    });
}

fn snapshot(name: &str, size: Vec2, mut draw: impl FnMut(&mut egui::Ui)) {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(1.0)
        .wgpu()
        .build_ui(|ui| draw(ui));
    harness.run();
    // The images were made on Linux. Windows draws through another graphics
    // backend, which rounds the edges of a few letters differently.
    let options = egui_kittest::SnapshotOptions::new()
        .max_failed_pixels(egui_kittest::OsThreshold::new(0).windows(16));
    harness.snapshot_options(name, &options);
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

/// What a machine with two screens and a few programs open has to share.
fn share_choices() -> Choices {
    let choice = |id, title: &str, detail: &str| Choice {
        id,
        title: title.into(),
        detail: detail.into(),
    };
    Choices {
        screens: vec![
            choice(1, "Screen 1", "2560 × 1440 · main"),
            choice(2, "Screen 2", "1920 × 1080"),
        ],
        windows: vec![
            choice(10, "Rocket League (64-bit, DX11, Cooked)", "RocketLeague"),
            choice(
                11,
                "video.rs - flowerchat - Visual Studio Code and a title that goes on for a while",
                "Code",
            ),
            choice(12, "Inbox - Mozilla Firefox", "firefox"),
            choice(13, "Windows PowerShell", "WindowsTerminal"),
        ],
    }
}

#[test]
fn room_wide_choosing_what_to_share() {
    let mut room = room();
    room.me.share = OwnShare::Starting;
    let mut state = form();
    state.share_choices = Some(share_choices());
    let devices = devices();
    let env = env(&devices);
    snapshot("room_wide_choosing_what_to_share", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}

#[test]
fn room_narrow_choosing_what_to_share() {
    let mut room = room();
    room.me.share = OwnShare::Starting;
    let mut state = form();
    state.share_choices = Some(share_choices());
    let devices = devices();
    let env = env(&devices);
    snapshot("room_narrow_choosing_what_to_share", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &env, &mut state);
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
        picture: None,
        codecs: &Codec::ALL,
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
        picture: None,
        codecs: &Codec::ALL,
    };
    snapshot("room_narrow_audio", NARROW, |ui| {
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}

#[test]
fn room_wide_someone_sharing() {
    let room = room_with_share();
    let mut state = form();
    let devices = devices();
    snapshot("room_wide_someone_sharing", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_wide_sharing_our_screen() {
    let mut room = room();
    room.me.share = OwnShare::Sharing(sharing(&[3, 5]));
    let mut state = form();
    let devices = devices();
    snapshot("room_wide_sharing_our_screen", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_stream() {
    let room = room_watching();
    let mut state = form();
    let devices = devices();
    snapshot_with_picture("room_stream", WIDE, |ui, picture| {
        let env = Env {
            picture: Some(picture),
            codecs: &Codec::ALL,
            ..env(&devices)
        };
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}

#[test]
fn room_stream_before_the_first_frame() {
    let mut room = room_watching();
    room.watching.as_mut().unwrap().stats = WatchStats::default();
    room.peers[0].stats.video_receiving_kbps = None;
    room.peers[0].stats.video_loss_percent = None;
    room.peers[0].stats.sound_receiving_kbps = None;
    room.peers[0].sharing.as_mut().unwrap().sound = false;
    let mut state = form();
    state.beside_screen = views::Beside::Audio;
    let devices = devices();
    snapshot("room_stream_before_the_first_frame", WIDE, |ui| {
        views::draw(ui, Screen::Room(&room), &env(&devices), &mut state);
    });
}

#[test]
fn room_maximized() {
    let room = room_watching();
    let mut state = form();
    state.maximized = true;
    // One message came in after the chat was last looked at.
    state.messages_seen = 3;
    let devices = devices();
    snapshot_with_picture("room_maximized", WIDE, |ui, picture| {
        let env = Env {
            picture: Some(picture),
            codecs: &Codec::ALL,
            ..env(&devices)
        };
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}

#[test]
fn room_narrow_screens() {
    let room = room_watching();
    let mut state = form();
    state.tab = Tab::Screens;
    state.messages_seen = 3;
    let devices = devices();
    snapshot_with_picture("room_narrow_screens", NARROW, |ui, picture| {
        let env = Env {
            picture: Some(picture),
            codecs: &Codec::ALL,
            ..env(&devices)
        };
        views::draw(ui, Screen::Room(&room), &env, &mut state);
    });
}
