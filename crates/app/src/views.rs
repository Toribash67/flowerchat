//! Draws the interface from a model and reports what the user did.
//!
//! Nothing here talks to the session. `draw` reads a [`Screen`], keeps
//! half-typed input in [`UiState`], and returns [`Action`]s for the caller
//! to carry out.

use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Button, CentralPanel, ComboBox, CornerRadius, FontId, Frame, Layout, Margin,
    Panel, Rect, RichText, ScrollArea, Sense, Slider, Stroke, StrokeKind, TextEdit, Ui, Vec2,
    WidgetInfo, WidgetType,
};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use flowerchat_client::audio::{AudioDevice, AudioDevices};
use flowerchat_client::session::{ConnectionState, Route};
use flowerchat_protocol::PeerId;

use crate::model::{Entry, EntryKind, PeerRow, RoomModel};
use crate::settings::{DeviceChoice, MAX_RECEIVE_BUFFER_MS, SendMode, Settings};
use crate::talk::GlobalKey;
use crate::theme::{self, muted, strong};

const SIDE_WIDTH: f32 = 232.0;
const CHAT_WIDTH: f32 = 300.0;
const AUDIO_WIDTH: f32 = 280.0;
const BIG_BUTTON_HEIGHT: f32 = 30.0;
const ICON_SIZE: f32 = 16.0;

pub enum Screen<'a> {
    /// Not in a room. `connecting` is set while a join is under way, and
    /// `error` holds why the last attempt or session ended, if it failed.
    Lobby {
        connecting: bool,
        error: Option<&'a str>,
        /// Microphone level from 0 (silent) to 1 (full scale).
        mic_level: f32,
    },
    Room(&'a RoomModel),
}

/// What the views show that belongs to the machine and not to a room.
pub struct Env<'a> {
    /// The audio devices that are plugged in.
    pub devices: &'a AudioDevices,
    /// Where the system-wide push-to-talk shortcut stands.
    pub global_key: GlobalKey,
    /// Whether the microphone is let out right now, as far as the choice
    /// between open mic and push-to-talk goes.
    pub transmitting: bool,
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
    /// The audio panel's choices. The caller watches these for changes.
    pub settings: Settings,
    /// Set while the next key press is to become the push-to-talk key.
    pub capturing_key: bool,
    /// The receive buffer while its slider is being dragged. It becomes a
    /// setting when the slider is let go.
    pub buffer_draft: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    People,
    Chat,
    Audio,
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
    /// Play a sound on the chosen speakers.
    TestSpeakers,
}

/// Whether the push-to-talk key chosen in the window is being held. Typing
/// that key into a text field does not count.
pub fn talk_key_down(ctx: &egui::Context, state: &UiState) -> bool {
    let Some(key) = state.settings.talk_key() else {
        return false;
    };
    !state.capturing_key && !ctx.text_edit_focused() && ctx.input(|input| input.key_down(key))
}

pub fn draw(ui: &mut Ui, screen: Screen<'_>, env: &Env<'_>, state: &mut UiState) -> Vec<Action> {
    let mut actions = Vec::new();
    if theme::install(ui.ctx()) {
        // The fonts are not usable until the next frame.
        ui.ctx().request_repaint();
        return actions;
    }
    let wide = ui.available_width() >= theme::NARROW_BELOW;
    match screen {
        Screen::Lobby {
            connecting,
            error,
            mic_level,
        } if wide => {
            side_panel("lobby").show(ui, |ui| {
                app_name(ui);
                join_form(ui, connecting, error, state, &mut actions);
            });
            Panel::right("lobby-audio")
                .exact_size(AUDIO_WIDTH)
                .resizable(false)
                .frame(
                    Frame::new()
                        .fill(theme::PANEL)
                        .inner_margin(Margin::symmetric(16, 14)),
                )
                .show(ui, |ui| {
                    ui.label(strong("Audio"));
                    ui.add_space(8.0);
                    audio_stacked(ui, env, state, &mut actions, Some(mic_level));
                });
            CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() / 2.0 - 40.0).max(0.0));
                    ui.set_max_width(360.0);
                    ui.label(strong("Not in a room").size(15.0));
                    ui.label(muted(
                        "Enter a name and a room on the left. The audio panel on the right \
                         already works, so you can check your microphone before joining.",
                    ));
                });
            });
        }
        Screen::Lobby {
            connecting, error, ..
        } => {
            CentralPanel::default()
                .frame(Frame::new().fill(theme::BACKGROUND).inner_margin(20))
                .show(ui, |ui| {
                    ui.set_max_width(320.0);
                    app_name(ui);
                    join_form(ui, connecting, error, state, &mut actions);
                });
        }
        Screen::Room(room) if wide => room_wide(ui, room, env, state, &mut actions),
        Screen::Room(room) => room_narrow(ui, room, env, state, &mut actions),
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

fn room_wide(
    ui: &mut Ui,
    room: &RoomModel,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let mode = mode_text(env, state);
    side_panel("room").show(ui, |ui| {
        Panel::bottom("me")
            .resizable(false)
            .frame(Frame::new().inner_margin(Margin {
                top: 12,
                ..Margin::ZERO
            }))
            .show(ui, |ui| me_block(ui, room, mode, actions));
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
                .show(ui, |ui| audio_columns(ui, env, state, actions));
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

// --- Room, narrow -----------------------------------------------------------

fn room_narrow(
    ui: &mut Ui,
    room: &RoomModel,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let bar = |margin: Margin| Frame::new().fill(theme::PANEL).inner_margin(margin);
    let mode = mode_text(env, state);
    let tab_button = |label: &str, selected: bool| {
        if selected {
            Button::new(strong(label)).fill(theme::CONTROL_HOVER)
        } else {
            Button::new(RichText::new(label).color(theme::MUTED)).fill(egui::Color32::TRANSPARENT)
        }
    };

    Panel::top("header")
        .resizable(false)
        .frame(bar(Margin::symmetric(12, 8)))
        .show(ui, |ui| {
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                // The buttons are placed first, from the right, so that it
                // is the status that gives way when space runs out.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if ui.button("Leave").clicked() {
                        actions.push(Action::Leave);
                    }
                    let audio = state.tab == Tab::Audio;
                    if ui.add(tab_button("Audio", audio)).clicked() {
                        state.tab = if audio { Tab::People } else { Tab::Audio };
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(strong(&room.room).size(15.0));
                        let status = format!(
                            "Connected · {} · {}",
                            room.peers.len() + 1,
                            clock(room.in_room)
                        );
                        ui.add(egui::Label::new(muted(status)).truncate());
                    });
                });
            });
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                for (tab, label) in [(Tab::People, "People"), (Tab::Chat, "Chat")] {
                    if ui.add(tab_button(label, state.tab == tab)).clicked() {
                        state.tab = tab;
                    }
                }
            });
        });

    Panel::bottom("me")
        .resizable(false)
        .frame(bar(Margin::symmetric(12, 10)))
        .show(ui, |ui| me_block(ui, room, mode, actions));

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
            Tab::Audio => {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.add_space(4.0);
                    audio_stacked(ui, env, state, actions, None);
                });
            }
        });
}

// --- Audio settings ---------------------------------------------------------

/// The audio settings side by side, for under the people table.
fn audio_columns(ui: &mut Ui, env: &Env<'_>, state: &mut UiState, actions: &mut Vec<Action>) {
    ui.label(strong("Audio"));
    ui.add_space(2.0);
    ui.spacing_mut().item_spacing.x = 20.0;
    ui.columns(3, |columns| {
        let [devices, processing, send] = columns else {
            return;
        };
        // Columns stretch what is put in them to their full width, which
        // suits neither text nor small controls.
        let column = |ui: &mut Ui, contents: &mut dyn FnMut(&mut Ui)| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                contents(ui);
            });
        };
        column(devices, &mut |ui| {
            device_pickers(ui, env, state, actions, None);
        });
        column(processing, &mut |ui| {
            microphone_processing(ui, state);
            ui.add_space(4.0);
            receive_buffer(ui, state, false);
        });
        column(send, &mut |ui| send_voice(ui, env, state));
    });
}

/// The audio settings in one column. `mic_level` adds a meter under the
/// microphone, for when nothing else shows that it works.
fn audio_stacked(
    ui: &mut Ui,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    mic_level: Option<f32>,
) {
    device_pickers(ui, env, state, actions, mic_level);
    ui.add_space(10.0);
    microphone_processing(ui, state);
    ui.add_space(10.0);
    send_voice(ui, env, state);
    ui.add_space(10.0);
    receive_buffer(ui, state, true);
}

fn device_pickers(
    ui: &mut Ui,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    mic_level: Option<f32>,
) {
    let settings = &mut state.settings;
    ui.spacing_mut().item_spacing.y = 4.0;
    ui.label(muted("Microphone"));
    let width = ui.available_width();
    device_picker(
        ui,
        "microphone",
        &mut settings.microphone,
        &env.devices.inputs,
        width,
    );
    if let Some(level) = mic_level {
        ui.add_space(2.0);
        meter(ui, level);
    }
    ui.add_space(6.0);
    ui.label(muted("Speakers"));
    row(ui, theme::CONTROL_HEIGHT, |ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if ui.button("Test").clicked() {
                actions.push(Action::TestSpeakers);
            }
            let width = ui.available_width();
            device_picker(
                ui,
                "speakers",
                &mut settings.speakers,
                &env.devices.outputs,
                width,
            );
        });
    });
}

/// A drop-down with the system default first and then every device.
fn device_picker(
    ui: &mut Ui,
    id: &str,
    choice: &mut Option<DeviceChoice>,
    devices: &[AudioDevice],
    width: f32,
) {
    let default = match devices.iter().find(|device| device.is_default) {
        Some(device) => format!("System default ({})", device.label),
        None => "System default".to_owned(),
    };
    let selected = match &*choice {
        None => default.clone(),
        Some(choice) => match devices.iter().find(|device| device.id == choice.id) {
            Some(device) => device.label.clone(),
            // No list at all means the devices could not be looked up.
            None if devices.is_empty() => choice.label.clone(),
            None => format!("{} (not connected)", choice.label),
        },
    };
    ComboBox::from_id_salt(id)
        .width(width)
        // Room for a dozen devices before the list starts to scroll.
        .height(12.0 * theme::CONTROL_HEIGHT)
        .selected_text(selected)
        .truncate()
        .show_ui(ui, |ui| {
            if ui.selectable_label(choice.is_none(), default).clicked() {
                *choice = None;
            }
            for device in devices {
                let chosen = choice.as_ref().is_some_and(|choice| choice.id == device.id);
                if ui.selectable_label(chosen, &device.label).clicked() {
                    *choice = Some(DeviceChoice::from(device));
                }
            }
        });
}

fn microphone_processing(ui: &mut Ui, state: &mut UiState) {
    let settings = &mut state.settings;
    option_rows(ui);
    ui.label(muted("Microphone processing"));
    for (on, label) in [
        (&mut settings.echo_cancellation, "Echo cancellation"),
        (&mut settings.noise_suppression, "Noise suppression"),
        (&mut settings.automatic_volume, "Automatic volume"),
    ] {
        if option(ui, *on, Mark::Check, label).clicked() {
            *on = !*on;
        }
    }
}

fn send_voice(ui: &mut Ui, env: &Env<'_>, state: &mut UiState) {
    option_rows(ui);
    ui.label(muted("Send voice"));
    let send = &mut state.settings.send;
    for (mode, label) in [
        (SendMode::Always, "Always (open mic)"),
        (SendMode::WhileKeyHeld, "While a key is held"),
    ] {
        if option(ui, *send == mode, Mark::Dot, label).clicked() {
            *send = mode;
        }
    }
    let push_to_talk = *send == SendMode::WhileKeyHeld;
    if !push_to_talk {
        state.capturing_key = false;
    }

    if state.capturing_key {
        let pressed = ui.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } => Some(*key),
                _ => None,
            })
        });
        if let Some(key) = pressed {
            if key != egui::Key::Escape {
                state.settings.talk_key = Some(key.name().to_owned());
            }
            state.capturing_key = false;
        }
    }

    ui.add_enabled_ui(push_to_talk, |ui| {
        ui.spacing_mut().interact_size.y = theme::CONTROL_HEIGHT;
        row(ui, theme::CONTROL_HEIGHT, |ui| {
            // In line with the labels of the options above.
            ui.add_space(OPTION_MARK + OPTION_GAP);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let label = if state.capturing_key {
                    "Cancel"
                } else {
                    "Set key"
                };
                if ui.button(label).clicked() {
                    state.capturing_key = !state.capturing_key;
                }
                let key = state.settings.talk_key();
                let (text, color) = match key {
                    _ if state.capturing_key => ("Press a key…", theme::TEXT),
                    Some(key) => (key.name(), theme::TEXT),
                    None => ("No key set", theme::MUTED),
                };
                readout(ui, text, color);
            });
        });
    });
    if push_to_talk {
        let note = match &env.global_key {
            GlobalKey::Off => None,
            GlobalKey::Registering => Some("Asking the system for a shortcut…".to_owned()),
            GlobalKey::Ready { trigger } if trigger.is_empty() => Some(
                "Works while this window has focus. For everywhere else, bind \
                 flowerchat's “push-to-talk” shortcut in your desktop's settings."
                    .to_owned(),
            ),
            GlobalKey::Ready { trigger } => Some(format!(
                "Works while this window has focus. Everywhere else: {trigger}."
            )),
            GlobalKey::Unavailable(reason) => Some(format!(
                "Works while this window has focus. Nowhere else, because {reason}."
            )),
        };
        if let Some(note) = note {
            ui.add_space(2.0);
            ui.label(muted(note));
        }
    }
}

/// `hint` adds a line on what the setting trades off.
fn receive_buffer(ui: &mut Ui, state: &mut UiState, hint: bool) {
    let mut value = state
        .buffer_draft
        .unwrap_or(state.settings.receive_buffer_ms);
    ui.spacing_mut().item_spacing.y = 2.0;
    row(ui, 18.0, |ui| {
        ui.label(muted("Receive buffer"));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("{value} ms")).size(theme::SMALL));
        });
    });
    ui.spacing_mut().slider_width = ui.available_width();
    // The handle's size follows this height.
    ui.spacing_mut().interact_size.y = 18.0;
    let slider = ui.add(Slider::new(&mut value, 0..=MAX_RECEIVE_BUFFER_MS).show_value(false));
    if slider.dragged() {
        state.buffer_draft = Some(value);
    } else if slider.drag_stopped() || slider.changed() {
        state.buffer_draft = None;
        state.settings.receive_buffer_ms = value;
    }
    if hint {
        ui.add_space(2.0);
        ui.label(muted("Lower means less delay. Raise it if voices cut out."));
    }
}

/// Spacing for a short list of checkboxes or radio buttons under a label.
fn option_rows(ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 3.0;
}

const OPTION_MARK: f32 = 14.0;
const OPTION_GAP: f32 = 8.0;

/// What shows that an option is on.
#[derive(Clone, Copy, PartialEq)]
enum Mark {
    /// A checkbox: any number of these can be on.
    Check,
    /// A radio button: one of a group is on.
    Dot,
}

/// A checkbox or radio button with its label. The caller changes the value
/// when it is clicked.
fn option(ui: &mut Ui, on: bool, mark: Mark, label: &str) -> egui::Response {
    let text = ui.painter().layout_no_wrap(
        label.to_owned(),
        FontId::proportional(theme::BODY),
        theme::TEXT,
    );
    let size = Vec2::new(OPTION_MARK + OPTION_GAP + text.size().x, 20.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| {
        let kind = match mark {
            Mark::Check => WidgetType::Checkbox,
            Mark::Dot => WidgetType::RadioButton,
        };
        WidgetInfo::selected(kind, ui.is_enabled(), on, label)
    });

    let center = egui::pos2(rect.left() + OPTION_MARK / 2.0, rect.center().y);
    let outline = if response.hovered() || response.has_focus() {
        theme::MUTED
    } else {
        theme::IDLE
    };
    let painter = ui.painter();
    match mark {
        Mark::Check => {
            let square = Rect::from_center_size(center, Vec2::splat(OPTION_MARK));
            let radius = CornerRadius::same(3);
            if on {
                painter.rect_filled(square, radius, theme::ACCENT);
                let at = |x: f32, y: f32| square.left_top() + Vec2::new(x, y);
                painter.line(
                    vec![at(3.5, 7.5), at(6.0, 10.0), at(10.5, 4.5)],
                    Stroke::new(1.8, theme::ON_ACCENT),
                );
            } else {
                let stroke = Stroke::new(1.5, outline);
                painter.rect(square, radius, theme::FIELD, stroke, StrokeKind::Inside);
            }
        }
        Mark::Dot => {
            let radius = OPTION_MARK / 2.0 - 0.75;
            if on {
                painter.circle_stroke(center, radius, Stroke::new(1.5, theme::ACCENT));
                painter.circle_filled(center, 3.5, theme::ACCENT);
            } else {
                painter.circle(center, radius, theme::FIELD, Stroke::new(1.5, outline));
            }
        }
    }
    let text_at = egui::pos2(
        rect.left() + OPTION_MARK + OPTION_GAP,
        rect.center().y - text.size().y / 2.0,
    );
    painter.galley(text_at, text, theme::TEXT);
    response
}

/// A value shown in the shape of a text field, filling the width left.
fn readout(ui: &mut Ui, text: &str, color: egui::Color32) {
    let size = Vec2::new(ui.available_width(), theme::CONTROL_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(theme::RADIUS),
        theme::FIELD,
        Stroke::new(1.0, theme::BORDER),
        StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + Vec2::new(8.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(theme::BODY),
        color,
    );
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
fn me_block(ui: &mut Ui, room: &RoomModel, mode: &str, actions: &mut Vec<Action>) {
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
    kv(ui, "Mode", mode);
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

/// How the microphone is being sent, for the block about yourself.
fn mode_text(env: &Env<'_>, state: &UiState) -> &'static str {
    match state.settings.send {
        SendMode::Always => "Open mic",
        SendMode::WhileKeyHeld if env.transmitting => "Key held, sending",
        SendMode::WhileKeyHeld => "Hold key to talk",
    }
}

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
