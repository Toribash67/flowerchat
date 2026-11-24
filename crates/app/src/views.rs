//! Draws the interface from a model and reports what the user did.
//!
//! Nothing here talks to the session. `draw` reads a [`Screen`], keeps
//! half-typed input in [`UiState`], and returns [`Action`]s for the caller
//! to carry out.

use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Button, CentralPanel, CornerRadius, FontId, Frame, Layout, Margin, Panel,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, Vec2,
};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use flowerchat_client::session::{ConnectionState, Route};
use flowerchat_protocol::PeerId;

use crate::model::{Entry, EntryKind, PeerRow, RoomModel};
use crate::theme::{self, muted, strong};

const SIDE_WIDTH: f32 = 232.0;
const CHAT_WIDTH: f32 = 300.0;
const BIG_BUTTON_HEIGHT: f32 = 30.0;
const ICON_SIZE: f32 = 16.0;

pub enum Screen<'a> {
    /// Not in a room. `connecting` is set while a join is under way, and
    /// `error` holds why the last attempt or session ended, if it failed.
    Lobby {
        connecting: bool,
        error: Option<&'a str>,
    },
    Room(&'a RoomModel),
}

/// Input that lives between frames but is not part of the room.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub name: String,
    pub room: String,
    pub server: String,
    pub chat_draft: String,
    /// Which part of the room a narrow window shows.
    pub tab: Tab,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    People,
    Chat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Join with the name, room and server in [`UiState`].
    Join,
    Leave,
    SetMuted(bool),
    SetDeafened(bool),
    SetPeerVolume {
        id: PeerId,
        volume: f32,
    },
    SendChat(String),
}

pub fn draw(ui: &mut Ui, screen: Screen<'_>, state: &mut UiState) -> Vec<Action> {
    let mut actions = Vec::new();
    if theme::install(ui.ctx()) {
        // The fonts are not usable until the next frame.
        ui.ctx().request_repaint();
        return actions;
    }
    let wide = ui.available_width() >= theme::NARROW_BELOW;
    match screen {
        Screen::Lobby { connecting, error } if wide => {
            side_panel("lobby").show(ui, |ui| {
                app_name(ui);
                join_form(ui, connecting, error, state, &mut actions);
            });
            CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(muted("Not in a room. Enter a name and a room on the left."));
                });
            });
        }
        Screen::Lobby { connecting, error } => {
            CentralPanel::default()
                .frame(Frame::new().fill(theme::BACKGROUND).inner_margin(20))
                .show(ui, |ui| {
                    ui.set_max_width(320.0);
                    app_name(ui);
                    join_form(ui, connecting, error, state, &mut actions);
                });
        }
        Screen::Room(room) if wide => room_wide(ui, room, state, &mut actions),
        Screen::Room(room) => room_narrow(ui, room, state, &mut actions),
    }
    actions
}

// --- Lobby -----------------------------------------------------------------

fn join_form(
    ui: &mut Ui,
    connecting: bool,
    error: Option<&str>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    ui.add_space(10.0);
    ui.label(theme::title("Join a room"));
    ui.add_space(4.0);

    let mut submitted = false;
    ui.add_enabled_ui(!connecting, |ui| {
        for (label, value) in [
            ("Name", &mut state.name),
            ("Room", &mut state.room),
            ("Server", &mut state.server),
        ] {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(muted(label));
            let field = ui.add(
                TextEdit::singleline(value)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 5)),
            );
            submitted |= field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
        }

        let ready = [&state.name, &state.room, &state.server]
            .iter()
            .all(|value| !value.trim().is_empty());
        let label = if connecting { "Joining…" } else { "Join" };
        let join = Button::new(strong(label).color(theme::ON_ACCENT)).fill(theme::ACCENT);
        let size = Vec2::new(ui.available_width(), BIG_BUTTON_HEIGHT);
        let clicked = ui
            .add_enabled(ready, |ui: &mut Ui| ui.add_sized(size, join))
            .clicked();
        if (clicked || submitted) && ready {
            actions.push(Action::Join);
        }
    });

    if let Some(error) = error {
        ui.add_space(8.0);
        ui.label(RichText::new(error).size(theme::SMALL).color(theme::WARN));
    }
}

// --- Room, wide -------------------------------------------------------------

fn room_wide(ui: &mut Ui, room: &RoomModel, state: &mut UiState, actions: &mut Vec<Action>) {
    side_panel("room").show(ui, |ui| {
        Panel::bottom("me")
            .resizable(false)
            .frame(Frame::new().inner_margin(Margin {
                top: 12,
                ..Margin::ZERO
            }))
            .show(ui, |ui| me_block(ui, room, actions));
        CentralPanel::no_frame().show(ui, |ui| {
            app_name(ui);
            ui.add_space(10.0);
            ui.label(theme::title(&room.room));
            ui.add_space(4.0);
            kv(ui, "Server", &room.server);
            kv(ui, "Status", "Connected");
            kv(ui, "People", (room.peers.len() + 1).to_string());
            kv(ui, "In room", clock(room.in_room));
            ui.add_space(6.0);
            if ui.button("Leave").clicked() {
                actions.push(Action::Leave);
            }
        });
    });

    Panel::right("chat")
        .exact_size(CHAT_WIDTH)
        .resizable(false)
        .frame(
            Frame::new()
                .fill(theme::PANEL)
                .inner_margin(Margin::symmetric(14, 12)),
        )
        .show(ui, |ui| chat(ui, room, state, actions, true));

    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme::BACKGROUND)
                .inner_margin(Margin::symmetric(16, 12)),
        )
        .show(ui, |ui| {
            Panel::bottom("audio")
                .resizable(false)
                .frame(Frame::new().inner_margin(Margin {
                    top: 10,
                    ..Margin::ZERO
                }))
                .show(ui, |ui| audio_summary(ui, room));
            CentralPanel::no_frame().show(ui, |ui| {
                row(ui, 20.0, |ui| {
                    ui.label(strong("People"));
                    ui.label(muted(headcount(room)));
                });
                ui.add_space(4.0);
                people_table(ui, room, actions);
            });
        });
}

fn people_table(ui: &mut Ui, room: &RoomModel, actions: &mut Vec<Action>) {
    let numeric = |ui: &mut Ui, text: RichText| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(text));
    };
    let heading = |text: &str| RichText::new(text).size(theme::TINY).color(theme::MUTED);

    TableBuilder::new(ui)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(18.0))
        .column(Column::remainder().at_least(80.0))
        .column(Column::exact(100.0))
        .column(Column::exact(64.0))
        .column(Column::exact(58.0))
        .column(Column::exact(50.0))
        .column(Column::exact(54.0))
        .column(Column::exact(76.0))
        // A gap, so the sliders do not run into the right-aligned figures.
        .column(Column::exact(8.0))
        .column(Column::exact(136.0))
        .header(24.0, |mut header| {
            header.col(|_| {});
            for text in ["Name", "Status", "Route"] {
                header.col(|ui| {
                    ui.label(heading(text));
                });
            }
            for text in ["Delay", "Loss", "Jitter", "Receiving"] {
                header.col(|ui| numeric(ui, heading(text)));
            }
            header.col(|_| {});
            header.col(|ui| {
                ui.label(heading("Volume"));
            });
        })
        .body(|mut body| {
            for peer in &room.peers {
                body.row(theme::ROW_HEIGHT, |mut cells| {
                    let stats = &peer.stats;
                    cells.col(|ui| indicator(ui, peer));
                    cells.col(|ui| {
                        ui.label(peer_name(peer));
                    });
                    cells.col(|ui| {
                        ui.label(status(peer));
                    });
                    cells.col(|ui| {
                        ui.label(figure(route(peer)));
                    });
                    cells.col(|ui| numeric(ui, figure(millis(stats.round_trip_ms))));
                    cells.col(|ui| numeric(ui, figure(percent(stats.loss_percent))));
                    cells.col(|ui| numeric(ui, figure(millis(stats.jitter_ms))));
                    cells.col(|ui| numeric(ui, figure(kbps(stats.receiving_kbps))));
                    cells.col(|_| {});
                    cells.col(|ui| volume(ui, peer, 82.0, actions));
                });
            }
        });
}

fn audio_summary(ui: &mut Ui, room: &RoomModel) {
    let audio = &room.audio;
    ui.label(strong("Audio"));
    ui.add_space(2.0);
    let processing = if audio.processing {
        "Echo cancellation, noise suppression, automatic volume"
    } else {
        "Off"
    };
    let buffer = format!("{} ms", audio.receive_buffer_ms);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 24.0;
        for (label, value) in [
            ("Microphone", audio.microphone.as_str()),
            ("Speakers", audio.speakers.as_str()),
            ("Microphone processing", processing),
            ("Receive buffer", buffer.as_str()),
        ] {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(muted(label));
                ui.label(RichText::new(value).size(theme::SMALL));
            });
        }
    });
}

// --- Room, narrow -----------------------------------------------------------

fn room_narrow(ui: &mut Ui, room: &RoomModel, state: &mut UiState, actions: &mut Vec<Action>) {
    let bar = |margin: Margin| Frame::new().fill(theme::PANEL).inner_margin(margin);

    Panel::top("header")
        .resizable(false)
        .frame(bar(Margin::symmetric(12, 8)))
        .show(ui, |ui| {
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                ui.label(strong(&room.room).size(15.0));
                ui.label(muted(format!(
                    "Connected · {} · {}",
                    room.peers.len() + 1,
                    clock(room.in_room)
                )));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Leave").clicked() {
                        actions.push(Action::Leave);
                    }
                });
            });
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                for (tab, label) in [(Tab::People, "People"), (Tab::Chat, "Chat")] {
                    let button = if state.tab == tab {
                        Button::new(strong(label)).fill(theme::CONTROL_HOVER)
                    } else {
                        Button::new(RichText::new(label).color(theme::MUTED))
                            .fill(egui::Color32::TRANSPARENT)
                    };
                    if ui.add(button).clicked() {
                        state.tab = tab;
                    }
                }
            });
        });

    Panel::bottom("me")
        .resizable(false)
        .frame(bar(Margin::symmetric(12, 10)))
        .show(ui, |ui| me_block(ui, room, actions));

    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme::BACKGROUND)
                .inner_margin(Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| match state.tab {
            Tab::People => {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    for peer in &room.peers {
                        row(ui, 40.0, |ui| {
                            indicator(ui, peer);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.add_space(3.0);
                                ui.label(peer_name(peer));
                                ui.label(muted(summary_line(peer)));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                // Laid out from the right, so the slider
                                // comes out left of its percentage.
                                volume_rtl(ui, peer, 72.0, actions);
                            });
                        });
                    }
                });
            }
            Tab::Chat => chat(ui, room, state, actions, false),
        });
}

// --- Shared pieces ----------------------------------------------------------

fn side_panel(id: &'static str) -> Panel {
    Panel::left(id)
        .exact_size(SIDE_WIDTH)
        .resizable(false)
        .frame(
            Frame::new()
                .fill(theme::PANEL)
                .inner_margin(Margin::symmetric(16, 14)),
        )
}

fn app_name(ui: &mut Ui) {
    ui.label(strong("flowerchat").color(theme::MUTED));
}

/// Your own status and controls: what you are sending, and mute and deafen.
fn me_block(ui: &mut Ui, room: &RoomModel, actions: &mut Vec<Action>) {
    let me = &room.me;
    row(ui, 20.0, |ui| {
        ui.label(strong(&me.name));
        ui.label(RichText::new("(you)").color(theme::MUTED));
        if me.muted {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let text = format!("{} Muted", icon::MICROPHONE_SLASH);
                ui.label(strong(text).size(theme::SMALL).color(theme::WARN));
            });
        }
    });
    meter(ui, if me.muted { 0.0 } else { me.mic_level });
    ui.add_space(2.0);
    kv(ui, "Sending", kbps(me.sending_kbps));
    kv(ui, "Codec", "Opus, 48 kHz");
    kv(ui, "Mode", "Open mic");
    ui.add_space(4.0);

    let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
    let size = Vec2::new(width, BIG_BUTTON_HEIGHT);
    ui.horizontal(|ui| {
        // "Undeafen" plus its icon needs nearly the whole button.
        ui.spacing_mut().button_padding.x = 4.0;
        let (label, glyph) = if me.muted {
            ("Unmute", icon::MICROPHONE_SLASH)
        } else {
            ("Mute", icon::MICROPHONE)
        };
        if ui.add_sized(size, toggle(glyph, label, me.muted)).clicked() {
            actions.push(Action::SetMuted(!me.muted));
        }
        let (label, glyph) = if me.deafened {
            ("Undeafen", icon::SPEAKER_SLASH)
        } else {
            ("Deafen", icon::SPEAKER_HIGH)
        };
        if ui
            .add_sized(size, toggle(glyph, label, me.deafened))
            .clicked()
        {
            actions.push(Action::SetDeafened(!me.deafened));
        }
    });
}

/// A button that turns orange while the state it controls is on.
fn toggle<'a>(glyph: &str, label: &str, on: bool) -> Button<'a> {
    if on {
        Button::new(icon_label(glyph, label, theme::ON_WARN, true)).fill(theme::WARN)
    } else {
        Button::new(icon_label(glyph, label, theme::TEXT, false))
    }
}

/// An icon followed by a label. The icon is drawn larger than the text,
/// because the glyphs are hard to tell apart at body size.
fn icon_label(glyph: &str, label: &str, color: egui::Color32, heavy: bool) -> LayoutJob {
    let format = |font_id| TextFormat {
        font_id,
        color,
        valign: Align::Center,
        ..Default::default()
    };
    let family = if heavy {
        theme::semibold()
    } else {
        egui::FontFamily::Proportional
    };
    let mut job = LayoutJob::default();
    job.append(glyph, 0.0, format(FontId::proportional(ICON_SIZE)));
    job.append(label, 5.0, format(FontId::new(theme::BODY, family)));
    job
}

/// The timeline and the message field. `titled` adds a heading, for layouts
/// where nothing else says that this is the chat.
fn chat(
    ui: &mut Ui,
    room: &RoomModel,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    titled: bool,
) {
    Panel::bottom("chat-input")
        .resizable(false)
        .show_separator_line(false)
        .frame(Frame::new().inner_margin(Margin {
            top: 8,
            ..Margin::ZERO
        }))
        .show(ui, |ui| {
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let send = ui.button("Send").clicked();
                    let field = ui.add(
                        TextEdit::singleline(&mut state.chat_draft)
                            .hint_text(format!("Message {}", room.room))
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(8, 5)),
                    );
                    let entered =
                        field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let text = state.chat_draft.trim();
                    if (send || entered) && !text.is_empty() {
                        actions.push(Action::SendChat(text.to_owned()));
                        state.chat_draft.clear();
                        field.request_focus();
                    }
                });
            });
        });

    CentralPanel::no_frame().show(ui, |ui| {
        if titled {
            ui.label(strong("Chat"));
            ui.add_space(4.0);
        }
        ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for entry in &room.timeline {
                    timeline_entry(ui, entry);
                }
            });
    });
}

fn timeline_entry(ui: &mut Ui, entry: &Entry) {
    match &entry.kind {
        EntryKind::Event(text) => {
            ui.label(muted(format!("{} · {text}", entry.time)));
        }
        EntryKind::Message { from, text } => {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(strong(from));
                    ui.label(
                        RichText::new(&entry.time)
                            .size(theme::TINY)
                            .color(theme::MUTED),
                    );
                });
                ui.label(text);
            });
        }
    }
}

/// Speaking indicator: a ringed dot while speaking, a hollow one otherwise.
fn indicator(ui: &mut Ui, peer: &PeerRow) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
    let center = rect.center();
    let painter = ui.painter();
    if !peer.connected() {
        painter.circle_stroke(center, 3.5, Stroke::new(1.5, theme::BORDER));
    } else if peer.muted {
        painter.text(
            center,
            Align2::CENTER_CENTER,
            icon::MICROPHONE_SLASH,
            FontId::proportional(14.0),
            theme::WARN,
        );
    } else if peer.speaking {
        painter.circle_filled(center, 3.5, theme::ACCENT);
        painter.circle_stroke(center, 6.5, Stroke::new(1.5, theme::ACCENT));
    } else {
        painter.circle_stroke(center, 3.5, Stroke::new(1.5, theme::IDLE));
    }
}

fn meter(ui: &mut Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    let radius = CornerRadius::same(3);
    ui.painter().rect_filled(rect, radius, theme::BORDER);
    if level > 0.0 {
        let mut filled = rect;
        filled.set_width(rect.width() * level.clamp(0.0, 1.0));
        ui.painter().rect_filled(filled, radius, theme::ACCENT);
    }
}

fn volume(ui: &mut Ui, peer: &PeerRow, width: f32, actions: &mut Vec<Action>) {
    if !peer.connected() {
        return;
    }
    volume_slider(ui, peer, width, actions);
    ui.label(muted(volume_text(peer)));
}

/// The same control for layouts that place widgets from the right.
fn volume_rtl(ui: &mut Ui, peer: &PeerRow, width: f32, actions: &mut Vec<Action>) {
    if !peer.connected() {
        return;
    }
    ui.add_sized([34.0, 16.0], egui::Label::new(muted(volume_text(peer))));
    volume_slider(ui, peer, width, actions);
}

fn volume_slider(ui: &mut Ui, peer: &PeerRow, width: f32, actions: &mut Vec<Action>) {
    let mut volume = peer.volume;
    ui.spacing_mut().slider_width = width;
    // The handle's size follows this height.
    ui.spacing_mut().interact_size.y = 18.0;
    let slider = egui::Slider::new(&mut volume, 0.0..=2.0).show_value(false);
    if ui.add(slider).changed() {
        actions.push(Action::SetPeerVolume {
            id: peer.id,
            volume,
        });
    }
}

fn volume_text(peer: &PeerRow) -> String {
    format!("{:.0}%", peer.volume * 100.0)
}

/// A label on the left and its value on the right.
fn kv(ui: &mut Ui, key: &str, value: impl Into<String>) {
    row(ui, 18.0, |ui| {
        ui.label(muted(key));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(value).size(theme::SMALL)).truncate());
        });
    });
}

/// A full-width horizontal strip of a fixed height, vertically centred.
fn row(ui: &mut Ui, height: f32, contents: impl FnOnce(&mut Ui)) {
    let size = Vec2::new(ui.available_width(), height);
    ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), contents);
}

// --- Wording ----------------------------------------------------------------

fn peer_name(peer: &PeerRow) -> RichText {
    if peer.connected() {
        strong(&peer.name)
    } else {
        RichText::new(&peer.name).color(theme::MUTED)
    }
}

fn status(peer: &PeerRow) -> RichText {
    let (text, color) = match peer.state {
        ConnectionState::Connecting => ("Connecting…", theme::MUTED),
        ConnectionState::Disconnected => ("Connection lost", theme::WARN),
        ConnectionState::Failed => ("Unreachable", theme::WARN),
        ConnectionState::Closed => ("Closed", theme::MUTED),
        ConnectionState::Connected if peer.muted => ("Muted", theme::WARN),
        ConnectionState::Connected if peer.speaking => ("Speaking", theme::TEXT),
        ConnectionState::Connected => ("Idle", theme::MUTED),
    };
    RichText::new(text).color(color)
}

fn route(peer: &PeerRow) -> String {
    match peer.stats.route {
        Some(Route::Direct) => "Direct".into(),
        Some(Route::Relayed) => "Relayed".into(),
        None => "–".into(),
    }
}

/// One line with what the table's columns would show, for narrow windows.
fn summary_line(peer: &PeerRow) -> String {
    if !peer.connected() {
        return status(peer).text().to_owned();
    }
    let stats = &peer.stats;
    [
        status(peer).text().to_owned(),
        route(peer).to_lowercase(),
        millis(stats.round_trip_ms),
        percent(stats.loss_percent),
    ]
    .into_iter()
    .filter(|part| part != "–")
    .collect::<Vec<_>>()
    .join(" · ")
}

fn headcount(room: &RoomModel) -> String {
    let total = room.peers.len() + 1;
    let connected = room.connected_count();
    match room.peers.len() {
        0 => "Only you so far".into(),
        others if others == connected => format!("{total} in the room"),
        others => format!("{total} in the room, {connected} of {others} connected"),
    }
}

/// A figure in a table cell; dashes for missing values are dimmed.
fn figure(text: String) -> RichText {
    let color = if text == "–" {
        theme::MUTED
    } else {
        theme::TEXT
    };
    RichText::new(text).color(color)
}

fn millis(value: Option<f64>) -> String {
    match value {
        Some(ms) if ms < 1.0 => "<1 ms".into(),
        Some(ms) => format!("{ms:.0} ms"),
        None => "–".into(),
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "–".into(), |v| format!("{v:.1}%"))
}

fn kbps(value: Option<f64>) -> String {
    value.map_or_else(|| "–".into(), |v| format!("{v:.0} kbps"))
}

fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_read_naturally() {
        assert_eq!(millis(Some(0.3)), "<1 ms");
        assert_eq!(millis(Some(17.6)), "18 ms");
        assert_eq!(millis(None), "–");
        assert_eq!(percent(Some(1.44)), "1.4%");
        assert_eq!(kbps(Some(57.8)), "58 kbps");
        assert_eq!(clock(Duration::from_secs(42 * 60 + 17)), "00:42:17");
    }
}
