//! Draws the interface from a model and reports what the user did.
//!
//! Nothing here talks to the session. `draw` reads a [`Screen`], keeps
//! half-typed input in [`UiState`], and returns [`Action`]s for the caller
//! to carry out.

use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Button, CentralPanel, ComboBox, CornerRadius, FontId, Frame, Layout, Margin,
    Panel, Popup, PopupCloseBehavior, Rect, RichText, ScrollArea, Sense, Slider, Stroke,
    StrokeKind, TextEdit, Ui, Vec2, WidgetInfo, WidgetType,
};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use flowerchat_client::audio::{AudioDevice, AudioDevices};
use flowerchat_client::session::{Codec, ConnectionState, Route};
use flowerchat_protocol::PeerId;

use crate::model::{Entry, EntryKind, OwnShare, PeerRow, RoomModel, Watching};
use crate::palettes;
use crate::screen::{Choice, Choices, Picked};
use crate::settings::{DeviceChoice, MAX_RECEIVE_BUFFER_MS, SendMode, Settings};
use crate::talk::GlobalKey;
use crate::theme::{self, Palette, muted, strong};

const SIDE_WIDTH: f32 = 232.0;
/// The left column while a screen is open: it holds the list of people too.
const STREAM_SIDE_WIDTH: f32 = 264.0;
const CHAT_WIDTH: f32 = 300.0;
/// Height of the bars above and below a shared screen.
const BAR_HEIGHT: f32 = 44.0;
const AUDIO_WIDTH: f32 = 280.0;
const BIG_BUTTON_HEIGHT: f32 = 30.0;
const ICON_SIZE: f32 = 16.0;
/// Width of the panel that asks what to share, where the window has room.
const PICKER_WIDTH: f32 = 480.0;
/// Height of a row in it, and in the list of palettes.
const CHOICE_HEIGHT: f32 = 28.0;
/// Width of the list of palettes.
const PALETTES_WIDTH: f32 = 230.0;

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
    /// The latest frame of the screen being watched, once there is one.
    pub picture: Option<Picture>,
    /// The codecs this machine can share a screen in.
    pub codecs: &'a [Codec],
}

/// A frame of a shared screen, ready to draw.
#[derive(Debug, Clone, Copy)]
pub struct Picture {
    pub texture: egui::TextureId,
    /// Width and height in pixels.
    pub size: Vec2,
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
    /// What the column beside a shared screen shows.
    pub beside_screen: Beside,
    /// Whether the shared screen fills the window.
    pub maximized: bool,
    /// Whether the chat is open beside a screen that fills the window.
    pub chat_while_maximized: bool,
    /// Whether the shared screen is shown in a window of its own. The
    /// caller draws that window.
    pub popped_out: bool,
    /// How many chat messages had arrived when the chat was last on screen.
    pub messages_seen: usize,
    /// How tall, in pixels of the display, the shared screen was last
    /// drawn. The caller asks for frames of about that size.
    pub picture_height: Option<u32>,
    /// What there is to share, while the user is being asked which. The
    /// caller puts it here and keeps it current, on systems that leave
    /// the asking to us.
    pub share_choices: Option<Choices>,
}

/// What the column beside a shared screen shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Beside {
    #[default]
    Chat,
    Audio,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    People,
    Chat,
    Screens,
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
    /// The same for the sound of a peer's shared screen.
    SetStreamVolume {
        id: PeerId,
        volume: f32,
    },
    SendChat(String),
    /// Play a sound on the chosen speakers.
    TestSpeakers,
    /// Let the user pick a screen and share it.
    ShareScreen,
    /// Share this one of [`UiState::share_choices`].
    Share(Picked),
    /// Share none of them after all.
    ShareNothing,
    StopSharing,
    /// Open this peer's shared screen, in place of any other.
    Watch(PeerId),
    StopWatching,
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
    theme::set_palette(state.settings.palette());
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
                app_name(ui, &mut state.settings);
                join_form(ui, connecting, error, state, &mut actions);
            });
            Panel::right("lobby-audio")
                .exact_size(AUDIO_WIDTH)
                .resizable(false)
                .frame(
                    Frame::new()
                        .fill(theme::colors().panel)
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
                .frame(
                    Frame::new()
                        .fill(theme::colors().background)
                        .inner_margin(20),
                )
                .show(ui, |ui| {
                    ui.set_max_width(320.0);
                    app_name(ui, &mut state.settings);
                    join_form(ui, connecting, error, state, &mut actions);
                });
        }
        Screen::Room(room) => {
            if room.watching.is_none() {
                state.maximized = false;
                state.popped_out = false;
            }
            match &room.watching {
                Some(watching) if wide && state.maximized => {
                    room_maximized(ui, room, watching, env, state, &mut actions);
                }
                Some(watching) if wide => {
                    room_stream(ui, room, watching, env, state, &mut actions);
                }
                _ if wide => room_wide(ui, room, env, state, &mut actions),
                _ => room_narrow(ui, room, env, state, &mut actions),
            }
            if let Some(choices) = &state.share_choices {
                share_picker(ui, choices, &mut actions);
            }
        }
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
        let join = Button::new(strong(label).color(theme::colors().on_accent))
            .fill(theme::colors().accent);
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
        ui.label(
            RichText::new(error)
                .size(theme::SMALL)
                .color(theme::colors().warn),
        );
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
            .show(ui, |ui| {
                let screen = Some((&mut state.settings, env.codecs));
                me_block(ui, room, mode, screen, actions);
            });
        CentralPanel::no_frame().show(ui, |ui| {
            app_name(ui, &mut state.settings);
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
                .fill(theme::colors().panel)
                .inner_margin(Margin::symmetric(14, 12)),
        )
        .show(ui, |ui| chat(ui, room, state, actions, true));

    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme::colors().background)
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
    let heading = |text: &str| {
        RichText::new(text)
            .size(theme::TINY)
            .color(theme::colors().muted)
    };

    TableBuilder::new(ui)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(18.0))
        .column(Column::remainder().at_least(64.0))
        .column(Column::exact(96.0))
        .column(Column::exact(56.0))
        .column(Column::exact(52.0))
        .column(Column::exact(44.0))
        .column(Column::exact(48.0))
        .column(Column::exact(70.0))
        .column(Column::exact(70.0))
        .column(Column::exact(118.0))
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
            // Left-aligned text straight after right-aligned figures needs
            // some air.
            for text in ["Screen", "Volume"] {
                header.col(|ui| {
                    ui.add_space(6.0);
                    ui.label(heading(text));
                });
            }
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
                    cells.col(|ui| {
                        ui.add_space(6.0);
                        if peer.sharing.is_none() {
                            ui.label(figure("–".into()));
                        } else if watch_button(ui, peer).clicked() {
                            actions.push(Action::Watch(peer.id));
                        }
                    });
                    cells.col(|ui| {
                        ui.add_space(6.0);
                        volume(ui, peer, 62.0, actions);
                    });
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
    let bar = |margin: Margin| {
        Frame::new()
            .fill(theme::colors().panel)
            .inner_margin(margin)
    };
    let mode = mode_text(env, state);

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
                    palette_picker(ui, &mut state.settings);
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
                let chat = match unread(room, state) {
                    0 => "Chat".to_owned(),
                    new => format!("Chat · {new} new"),
                };
                let screens = match room.sharing_count() {
                    0 => "Screens".to_owned(),
                    shared => format!("Screens · {shared}"),
                };
                for (tab, label) in [
                    (Tab::People, "People"),
                    (Tab::Chat, chat.as_str()),
                    (Tab::Screens, screens.as_str()),
                ] {
                    if ui.add(tab_button(label, state.tab == tab)).clicked() {
                        state.tab = tab;
                    }
                }
            });
        });

    Panel::bottom("me")
        .resizable(false)
        .frame(bar(Margin::symmetric(12, 10)))
        // The Screens tab has what a wider window shows here about sharing.
        .show(ui, |ui| me_block(ui, room, mode, None, actions));

    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme::colors().background)
                .inner_margin(Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| match state.tab {
            Tab::People => {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    people_rows(ui, room, 72.0, actions);
                });
            }
            Tab::Chat => chat(ui, room, state, actions, false),
            Tab::Screens => screens_tab(ui, room, env, state, actions),
            Tab::Audio => {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.add_space(4.0);
                    audio_stacked(ui, env, state, actions, None);
                });
            }
        });
}

/// The people in the room as a list, for where there is no room for the
/// table.
fn people_rows(ui: &mut Ui, room: &RoomModel, slider_width: f32, actions: &mut Vec<Action>) {
    let watched = room.watching.as_ref().map(|watching| watching.id);
    for peer in &room.peers {
        row(ui, 40.0, |ui| {
            indicator(ui, peer);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.label(peer_name(peer));
                    if peer.sharing.is_some() {
                        let tag = |text: &str| {
                            strong(text).size(theme::TINY).color(theme::colors().accent)
                        };
                        if watched == Some(peer.id) {
                            ui.label(tag("· sharing"));
                        } else {
                            let link = Button::new(tag("· watch screen")).frame(false);
                            if ui.add(link).clicked() {
                                actions.push(Action::Watch(peer.id));
                            }
                        }
                    }
                });
                ui.label(muted(summary_line(peer)));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Laid out from the right, so the slider comes out left of
                // its percentage.
                volume_rtl(ui, peer, slider_width, actions);
            });
        });
    }
}

// --- Room, with a shared screen open ----------------------------------------

/// The room around someone's shared screen.
fn room_stream(
    ui: &mut Ui,
    room: &RoomModel,
    watching: &Watching,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let mode = mode_text(env, state);
    Panel::left("stream-people")
        .exact_size(STREAM_SIDE_WIDTH)
        .resizable(false)
        .frame(
            Frame::new()
                .fill(theme::colors().panel)
                .inner_margin(Margin::symmetric(14, 12)),
        )
        .show(ui, |ui| {
            Panel::bottom("me")
                .resizable(false)
                .frame(Frame::new().inner_margin(Margin {
                    top: 10,
                    ..Margin::ZERO
                }))
                .show(ui, |ui| {
                    let screen = Some((&mut state.settings, env.codecs));
                    me_block(ui, room, mode, screen, actions);
                });
            CentralPanel::no_frame().show(ui, |ui| {
                row(ui, 36.0, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Leave").clicked() {
                            actions.push(Action::Leave);
                        }
                        palette_picker(ui, &mut state.settings);
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
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
                ui.add_space(6.0);
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    people_rows(ui, room, 60.0, actions);
                });
            });
        });

    Panel::right("beside-screen")
        .exact_size(CHAT_WIDTH)
        .resizable(false)
        .frame(
            Frame::new()
                .fill(theme::colors().panel)
                .inner_margin(Margin::symmetric(14, 10)),
        )
        .show(ui, |ui| {
            Panel::top("beside-screen-tabs")
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().inner_margin(Margin {
                    bottom: 8,
                    ..Margin::ZERO
                }))
                .show(ui, |ui| {
                    row(ui, theme::CONTROL_HEIGHT, |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        for (beside, label) in [(Beside::Chat, "Chat"), (Beside::Audio, "Audio")] {
                            let selected = state.beside_screen == beside;
                            if ui.add(tab_button(label, selected)).clicked() {
                                state.beside_screen = beside;
                            }
                        }
                    });
                });
            match state.beside_screen {
                Beside::Chat => chat(ui, room, state, actions, false),
                Beside::Audio => {
                    CentralPanel::no_frame().show(ui, |ui| {
                        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                            audio_stacked(ui, env, state, actions, None);
                        });
                    });
                }
            }
        });

    let name = sharer_name(room, watching);
    let figures = StreamFigures::of(room, watching, state);
    CentralPanel::default()
        .frame(Frame::new().fill(theme::colors().background))
        .show(ui, |ui| {
            Panel::top("stage-header")
                .exact_size(BAR_HEIGHT)
                .resizable(false)
                .frame(Frame::new().inner_margin(Margin::symmetric(12, 0)))
                .show(ui, |ui| {
                    row(ui, BAR_HEIGHT, |ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let maximize = Button::new(icon_label(
                                icon::CORNERS_OUT,
                                "Maximize",
                                theme::colors().on_accent,
                                true,
                            ))
                            .fill(theme::colors().accent);
                            if ui.add(maximize).clicked() {
                                state.maximized = true;
                            }
                            pop_out_button(ui, state);
                            if ui.button("Stop watching").clicked() {
                                actions.push(Action::StopWatching);
                            }
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.label(strong(format!("{name}'s screen")));
                                let watchers = room.watchers_of(watching.id);
                                let text =
                                    format!("{} watching: {}", watchers.len(), watchers.join(", "));
                                ui.add(egui::Label::new(muted(text)).truncate());
                            });
                        });
                    });
                });
            Panel::bottom("stage-figures")
                .resizable(false)
                .frame(Frame::new().inner_margin(Margin::symmetric(16, 12)))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 24.0;
                    ui.columns(3, |columns| {
                        let [picture, delivery, sound] = columns else {
                            return;
                        };
                        stream_sound(sound, room, watching, &figures, actions);
                        figure_group(
                            picture,
                            "Picture",
                            &[
                                ("Resolution", &figures.resolution),
                                ("Frame rate", &figures.frame_rate),
                                ("Codec", &figures.codec),
                                ("Bitrate", &figures.bitrate),
                            ],
                        );
                        figure_group(
                            delivery,
                            "Delivery",
                            &[
                                ("Delay", &figures.delay),
                                ("Frames dropped", &figures.frames_dropped),
                                ("Packets lost", &figures.packets_lost),
                                ("Route", &figures.route),
                            ],
                        );
                    });
                });
            CentralPanel::no_frame().show(ui, |ui| stage(ui, &name, env.picture, state));
        });
}

/// The shared screen across the whole window, with the room reduced to a
/// bar above and a bar below.
fn room_maximized(
    ui: &mut Ui,
    room: &RoomModel,
    watching: &Watching,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    let bar = || {
        Frame::new()
            .fill(theme::colors().background)
            .inner_margin(Margin::symmetric(12, 0))
    };
    let name = sharer_name(room, watching);
    let figures = StreamFigures::of(room, watching, state);

    Panel::top("maximized-top")
        .exact_size(BAR_HEIGHT)
        .resizable(false)
        .frame(bar())
        .show(ui, |ui| {
            row(ui, BAR_HEIGHT, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let restore = Button::new(icon_label(
                        icon::CORNERS_IN,
                        "Restore",
                        theme::colors().text,
                        false,
                    ));
                    if ui.add(restore).clicked() {
                        state.maximized = false;
                    }
                    pop_out_button(ui, state);
                    if figures.sound
                        && let Some(peer) = room.peer(watching.id)
                    {
                        stream_volume_rtl(ui, peer, 90.0, actions);
                        ui.label(muted("Stream sound"));
                        ui.add_space(4.0);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        ui.label(strong(format!("{name}'s screen")));
                        let line = format!(
                            "{} · {} · {} · {} · delay {} · {} frames dropped",
                            figures.resolution,
                            figures.frame_rate,
                            figures.codec,
                            figures.bitrate,
                            figures.delay,
                            figures.frames_dropped
                        );
                        ui.add(egui::Label::new(muted(line)).truncate());
                    });
                });
            });
        });

    Panel::bottom("maximized-bottom")
        .exact_size(BAR_HEIGHT)
        .resizable(false)
        .frame(bar())
        .show(ui, |ui| {
            row(ui, BAR_HEIGHT, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let me = &room.me;
                    let (label, glyph) = if me.deafened {
                        ("Undeafen", icon::SPEAKER_SLASH)
                    } else {
                        ("Deafen", icon::SPEAKER_HIGH)
                    };
                    if ui.add(toggle(glyph, label, me.deafened)).clicked() {
                        actions.push(Action::SetDeafened(!me.deafened));
                    }
                    let (label, glyph) = if me.muted {
                        ("Unmute", icon::MICROPHONE_SLASH)
                    } else {
                        ("Mute", icon::MICROPHONE)
                    };
                    if ui.add(toggle(glyph, label, me.muted)).clicked() {
                        actions.push(Action::SetMuted(!me.muted));
                    }
                    let open = state.chat_while_maximized;
                    if ui.add(tab_button("Chat", open)).clicked() {
                        state.chat_while_maximized = !open;
                    }
                    match unread(room, state) {
                        0 => {}
                        1 => drop(ui.label(muted("1 new message"))),
                        new => drop(ui.label(muted(format!("{new} new messages")))),
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for peer in room.peers.iter().filter(|peer| peer.connected()) {
                            indicator(ui, peer);
                            let color = if peer.muted {
                                theme::colors().warn
                            } else if peer.speaking {
                                theme::colors().text
                            } else {
                                theme::colors().muted
                            };
                            ui.label(RichText::new(&peer.name).color(color));
                            ui.add_space(10.0);
                        }
                    });
                });
            });
        });

    if state.chat_while_maximized {
        Panel::right("maximized-chat")
            .exact_size(CHAT_WIDTH)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(theme::colors().panel)
                    .inner_margin(Margin::symmetric(14, 12)),
            )
            .show(ui, |ui| chat(ui, room, state, actions, true));
    }

    CentralPanel::default()
        .frame(Frame::new().fill(theme::colors().stage))
        .show(ui, |ui| stage(ui, &name, env.picture, state));
}

/// What the Screens tab of a narrow window shows: the screen being
/// watched, what the others share, and our own share.
fn screens_tab(
    ui: &mut Ui,
    room: &RoomModel,
    env: &Env<'_>,
    state: &mut UiState,
    actions: &mut Vec<Action>,
) {
    ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        if let Some(watching) = &room.watching {
            let name = sharer_name(room, watching);
            let width = ui.available_width();
            let size = Vec2::new(width, (width * 9.0 / 16.0).round());
            ui.allocate_ui(size, |ui| stage(ui, &name, env.picture, state));
            let figures = StreamFigures::of(room, watching, state);
            for (key, value) in [
                ("Resolution", &figures.resolution),
                ("Frame rate", &figures.frame_rate),
                ("Bitrate", &figures.bitrate),
                ("Delay", &figures.delay),
                ("Frames dropped", &figures.frames_dropped),
                ("Packets lost", &figures.packets_lost),
            ] {
                kv(ui, key, value.as_str());
            }
            if figures.sound
                && let Some(peer) = room.peer(watching.id)
            {
                row(ui, 20.0, |ui| {
                    ui.label(muted("Stream sound"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        stream_volume_rtl(ui, peer, 110.0, actions);
                    });
                });
            }
            ui.add_space(8.0);
        }

        ui.label(muted("Shared by others"));
        let watched = room.watching.as_ref().map(|watching| watching.id);
        let mut anyone = false;
        for peer in &room.peers {
            let Some(sharing) = &peer.sharing else {
                continue;
            };
            anyone = true;
            row(ui, 34.0, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if watched == Some(peer.id) {
                        if ui.button("Stop watching").clicked() {
                            actions.push(Action::StopWatching);
                        }
                    } else if watch_button(ui, peer).clicked() {
                        actions.push(Action::Watch(peer.id));
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(peer_name(peer));
                        let text = format!(
                            "{} · {} fps",
                            resolution(sharing.width, sharing.height),
                            sharing.fps
                        );
                        ui.add(egui::Label::new(muted(text)).truncate());
                    });
                });
            });
        }
        if !anyone {
            ui.label("Nobody else is sharing a screen.");
        }
        ui.add_space(8.0);

        ui.label(muted("Your screen"));
        own_share_figures(ui, room);
        ui.add_space(2.0);
        share_pickers(ui, room, &mut state.settings, env.codecs);
        share_button(ui, &room.me.share, actions);
    });
}

fn sharer_name(room: &RoomModel, watching: &Watching) -> String {
    room.peer(watching.id)
        .map_or_else(|| format!("peer {}", watching.id), |peer| peer.name.clone())
}

/// Moves the shared screen into a window of its own, or back.
fn pop_out_button(ui: &mut Ui, state: &mut UiState) {
    let label = if state.popped_out {
        "Bring back"
    } else {
        "Pop out"
    };
    if ui.button(label).clicked() {
        state.popped_out = !state.popped_out;
    }
}

/// The shared screen itself, as large as fits the space available.
fn stage(ui: &mut Ui, name: &str, picture: Option<Picture>, state: &mut UiState) {
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, theme::colors().stage);
    let note = match picture {
        _ if state.popped_out => format!("{name}'s screen is in its own window"),
        Some(picture) => {
            state.picture_height = Some(draw_picture(ui, rect, picture));
            return;
        }
        None => {
            // Until a frame shows the shape of the picture, the usual
            // shape of a screen is assumed.
            let fitted = fit(rect, Vec2::new(16.0, 9.0));
            state.picture_height = Some(display_pixels(ui, fitted.height()));
            format!("Waiting for {name}'s screen…")
        }
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        note,
        FontId::proportional(theme::BODY),
        theme::colors().muted,
    );
}

/// Draws `picture` in the middle of `within`, as large as fits without
/// changing its shape. Returns how tall that is in pixels of the display.
pub fn draw_picture(ui: &Ui, within: Rect, picture: Picture) -> u32 {
    let fitted = fit(within, picture.size);
    let whole = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    ui.painter()
        .image(picture.texture, fitted, whole, egui::Color32::WHITE);
    display_pixels(ui, fitted.height())
}

/// The largest rectangle of the given shape that fits in the middle of
/// `within`.
fn fit(within: Rect, shape: Vec2) -> Rect {
    let shape = shape.max(Vec2::splat(1.0));
    let scale = (within.width() / shape.x).min(within.height() / shape.y);
    Rect::from_center_size(within.center(), shape * scale)
}

/// A length of the interface in pixels of the display.
fn display_pixels(ui: &Ui, length: f32) -> u32 {
    (length * ui.ctx().pixels_per_point()).ceil().max(0.0) as u32
}

/// The sound that comes with a shared screen: how loud to play it, and
/// what arrives.
fn stream_sound(
    ui: &mut Ui,
    room: &RoomModel,
    watching: &Watching,
    figures: &StreamFigures,
    actions: &mut Vec<Action>,
) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 2.0);
        ui.label(
            strong("Stream sound")
                .size(theme::SMALL)
                .color(theme::colors().muted),
        );
        ui.add_space(4.0);
        let Some(peer) = room.peer(watching.id) else {
            return;
        };
        if !figures.sound {
            ui.label(muted(format!("{} shares without sound", peer.name)));
            return;
        }
        row(ui, 18.0, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let width = ui.available_width() - 34.0 - ui.spacing().item_spacing.x;
                stream_volume_rtl(ui, peer, width, actions);
            });
        });
        kv(ui, "Codec", "Opus, stereo");
        kv(ui, "Bitrate", figures.sound_bitrate.as_str());
    });
}

/// The volume of a shared screen's sound, for layouts that place widgets
/// from the right: the percentage, and the slider left of it.
fn stream_volume_rtl(ui: &mut Ui, peer: &PeerRow, width: f32, actions: &mut Vec<Action>) {
    let text = format!("{:.0}%", peer.stream_volume * 100.0);
    ui.add_sized([34.0, 16.0], egui::Label::new(muted(text)));
    let mut volume = peer.stream_volume;
    ui.spacing_mut().slider_width = width.max(20.0);
    // The handle's size follows this height.
    ui.spacing_mut().interact_size.y = 18.0;
    let slider = egui::Slider::new(&mut volume, 0.0..=2.0).show_value(false);
    if ui.add(slider).changed() {
        actions.push(Action::SetStreamVolume {
            id: peer.id,
            volume,
        });
    }
}

/// A heading with labelled figures under it.
fn figure_group(ui: &mut Ui, heading: &str, figures: &[(&str, &str)]) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 2.0);
        ui.label(
            strong(heading)
                .size(theme::SMALL)
                .color(theme::colors().muted),
        );
        ui.add_space(4.0);
        for (key, value) in figures {
            kv(ui, key, *value);
        }
    });
}

/// A button for one of several views, lit while its view is the one shown.
fn tab_button<'a>(label: &str, selected: bool) -> Button<'a> {
    if selected {
        Button::new(strong(label)).fill(theme::colors().control_hover)
    } else {
        Button::new(RichText::new(label).color(theme::colors().muted))
            .fill(egui::Color32::TRANSPARENT)
    }
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
                    _ if state.capturing_key => ("Press a key…", theme::colors().text),
                    Some(key) => (key.name(), theme::colors().text),
                    None => ("No key set", theme::colors().muted),
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
            GlobalKey::SameKey => {
                Some("Works in every window, not only while this one has focus.".to_owned())
            }
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
        theme::colors().text,
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
        theme::colors().muted
    } else {
        theme::colors().idle
    };
    let painter = ui.painter();
    match mark {
        Mark::Check => {
            let square = Rect::from_center_size(center, Vec2::splat(OPTION_MARK));
            let radius = CornerRadius::same(3);
            if on {
                painter.rect_filled(square, radius, theme::colors().accent);
                let at = |x: f32, y: f32| square.left_top() + Vec2::new(x, y);
                painter.line(
                    vec![at(3.5, 7.5), at(6.0, 10.0), at(10.5, 4.5)],
                    Stroke::new(1.8, theme::colors().on_accent),
                );
            } else {
                let stroke = Stroke::new(1.5, outline);
                painter.rect(
                    square,
                    radius,
                    theme::colors().field,
                    stroke,
                    StrokeKind::Inside,
                );
            }
        }
        Mark::Dot => {
            let radius = OPTION_MARK / 2.0 - 0.75;
            if on {
                painter.circle_stroke(center, radius, Stroke::new(1.5, theme::colors().accent));
                painter.circle_filled(center, 3.5, theme::colors().accent);
            } else {
                painter.circle(
                    center,
                    radius,
                    theme::colors().field,
                    Stroke::new(1.5, outline),
                );
            }
        }
    }
    let text_at = egui::pos2(
        rect.left() + OPTION_MARK + OPTION_GAP,
        rect.center().y - text.size().y / 2.0,
    );
    painter.galley(text_at, text, theme::colors().text);
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
        theme::colors().field,
        Stroke::new(1.0, theme::colors().border),
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
                .fill(theme::colors().panel)
                .inner_margin(Margin::symmetric(16, 14)),
        )
}

/// The name of the app, and beside it the button for its colours.
fn app_name(ui: &mut Ui, settings: &mut Settings) {
    row(ui, theme::CONTROL_HEIGHT, |ui| {
        ui.label(strong("flowerchat").color(theme::colors().muted));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            palette_picker(ui, settings);
        });
    });
}

/// The button that opens the list of palettes, and the list. A click on a
/// palette changes the colours at once and leaves the list open, so that
/// one after the other can be tried.
fn palette_picker(ui: &mut Ui, settings: &mut Settings) {
    let colors = theme::colors();
    let glyph = RichText::new(icon::PALETTE).size(ICON_SIZE);
    let button = ui
        .add(Button::new(glyph).min_size(Vec2::splat(theme::CONTROL_HEIGHT)))
        .on_hover_text("Colours");
    button.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Colours"));
    let chosen = settings.palette();
    // As many as fit in the window, and the rest by scrolling.
    let room = ui.ctx().content_rect().height() - 2.0 * BAR_HEIGHT;
    let height = room.clamp(3.0 * CHOICE_HEIGHT, 12.5 * CHOICE_HEIGHT);
    let frame = Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1.0, colors.border))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::same(6));
    Popup::menu(&button)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .frame(frame)
        .width(PALETTES_WIDTH)
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ScrollArea::vertical().max_height(height).show(ui, |ui| {
                for palette in &palettes::ALL {
                    if palette_row(ui, palette, palette == chosen).clicked() {
                        settings.palette = palette.id.to_owned();
                    }
                }
            });
        });
}

/// One palette to choose: a taste of its colours, its name, and a mark on
/// the one in use.
fn palette_row(ui: &mut Ui, palette: &Palette, chosen: bool) -> egui::Response {
    const PAD: f32 = 8.0;
    const SWATCH: Vec2 = Vec2::new(38.0, 16.0);
    let colors = theme::colors();
    let size = Vec2::new(ui.available_width(), CHOICE_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, true, chosen, palette.name));
    let painter = ui.painter();
    if response.hovered() || response.has_focus() {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS),
            colors.control_hover,
        );
    }
    // Its background, with its text, its accent and its warning on it.
    let swatch = Rect::from_min_size(
        egui::pos2(rect.left() + PAD, rect.center().y - SWATCH.y / 2.0),
        SWATCH,
    );
    painter.rect(
        swatch,
        CornerRadius::same(theme::RADIUS),
        palette.colors.background,
        Stroke::new(1.0, colors.border),
        StrokeKind::Inside,
    );
    let dots = [
        palette.colors.text,
        palette.colors.accent,
        palette.colors.warn,
    ];
    for (place, color) in dots.into_iter().enumerate() {
        let x = swatch.left() + SWATCH.x * (place as f32 + 1.0) / 4.0;
        painter.circle_filled(egui::pos2(x, swatch.center().y), 3.0, color);
    }
    painter.text(
        egui::pos2(swatch.right() + PAD, rect.center().y),
        Align2::LEFT_CENTER,
        palette.name,
        FontId::proportional(theme::BODY),
        colors.text,
    );
    if chosen {
        painter.text(
            rect.right_center() - Vec2::new(PAD, 0.0),
            Align2::RIGHT_CENTER,
            icon::CHECK,
            FontId::proportional(ICON_SIZE),
            colors.accent,
        );
    }
    response
}

/// Your own status and controls: what you are sending, mute and deafen,
/// and with `screen`, which holds the choices for it and the codecs there
/// are to choose from, also what you share of your screen.
fn me_block(
    ui: &mut Ui,
    room: &RoomModel,
    mode: &str,
    screen: Option<(&mut Settings, &[Codec])>,
    actions: &mut Vec<Action>,
) {
    let me = &room.me;
    row(ui, 20.0, |ui| {
        ui.label(strong(&me.name));
        ui.label(RichText::new("(you)").color(theme::colors().muted));
        if me.muted {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let text = format!("{} Muted", icon::MICROPHONE_SLASH);
                ui.label(strong(text).size(theme::SMALL).color(theme::colors().warn));
            });
        }
    });
    meter(ui, if me.muted { 0.0 } else { me.mic_level });
    ui.add_space(2.0);
    kv(ui, "Sending", kbps(me.sending_kbps));
    kv(ui, "Codec", "Opus, 48 kHz");
    kv(ui, "Mode", mode);
    if screen.is_some() {
        own_share_figures(ui, room);
    }
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
    if let Some((settings, codecs)) = screen {
        share_pickers(ui, room, settings, codecs);
        share_button(ui, &me.share, actions);
    }
}

/// What we share of our screen, and with whom.
fn own_share_figures(ui: &mut Ui, room: &RoomModel) {
    kv(ui, "Screen", own_share_text(&room.me.share));
    if let OwnShare::Sharing(sharing) = &room.me.share {
        let video = format!(
            "{} · {}",
            sharing.codec.label(),
            bitrate(Some(f64::from(sharing.kbps)))
        );
        kv(ui, "Video", video);
        kv(ui, "Encoded in", format!("{:.0} ms", sharing.encode_ms));
        kv(
            ui,
            "Watched by",
            name_list(&room.my_watchers(), "Nobody yet"),
        );
    }
}

/// The heights a shared screen can be limited to. `None` sends it at the
/// size it has.
const SHARE_HEIGHTS: [Option<u32>; 5] = [None, Some(2160), Some(1440), Some(1080), Some(720)];
const SHARE_RATES: [u32; 2] = [30, 60];
/// The bitrates a shared screen can be held to, in Mbit/s.
const SHARE_MBPS: [u32; 12] = [1, 2, 3, 4, 6, 8, 10, 15, 20, 30, 40, 50];

/// The size, frame rate, bitrate and codec to share a screen at, the last
/// from among `codecs`. All can be changed while sharing.
fn share_pickers(ui: &mut Ui, room: &RoomModel, settings: &mut Settings, codecs: &[Codec]) {
    // What the machine plays, with the call itself left out.
    if option(ui, settings.share_sound, Mark::Check, "With sound").clicked() {
        settings.share_sound = !settings.share_sound;
    }
    let source = match &room.me.share {
        OwnShare::Sharing(sharing) => Some((sharing.source_width, sharing.source_height)),
        _ => None,
    };
    let height_label = |choice: Option<u32>| match (choice, source) {
        (None, Some((width, height))) => format!("As it is ({})", resolution(width, height)),
        (None, None) => "As it is".to_owned(),
        (Some(height), _) => format!("Up to {height}p"),
    };
    row(ui, theme::CONTROL_HEIGHT, |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let width = ui.available_width() - 6.0;
        ComboBox::from_id_salt("share-height")
            .width((width * 0.6).floor())
            .selected_text(height_label(settings.share_height))
            .truncate()
            .show_ui(ui, |ui| {
                for choice in SHARE_HEIGHTS {
                    let chosen = settings.share_height == choice;
                    if ui.selectable_label(chosen, height_label(choice)).clicked() {
                        settings.share_height = choice;
                    }
                }
            });
        ComboBox::from_id_salt("share-fps")
            .width(ui.available_width())
            .selected_text(format!("{} fps", settings.share_fps))
            .truncate()
            .show_ui(ui, |ui| {
                for fps in SHARE_RATES {
                    let chosen = settings.share_fps == fps;
                    if ui.selectable_label(chosen, format!("{fps} fps")).clicked() {
                        settings.share_fps = fps;
                    }
                }
            });
    });

    // A codec chosen on another machine may not be one this one has.
    let mut codec = settings.share_codec();
    if !codecs.contains(&codec)
        && let Some(first) = codecs.first()
    {
        codec = *first;
        settings.share_codec = codec.id().into();
    }
    // Left to itself, the bitrate follows the size and frame rate. What it
    // comes to is among the figures above while sharing.
    let automatic = || "Auto bitrate".to_owned();
    let set_bitrate = |kbps: u32| match kbps % 1000 {
        0 => format!("{} Mbps", kbps / 1000),
        _ => bitrate(Some(f64::from(kbps))),
    };
    row(ui, theme::CONTROL_HEIGHT, |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let width = ui.available_width() - 6.0;
        ComboBox::from_id_salt("share-bitrate")
            .width((width * 0.6).floor())
            .selected_text(settings.share_kbps.map_or_else(automatic, set_bitrate))
            .truncate()
            .show_ui(ui, |ui| {
                let chosen = settings.share_kbps.is_none();
                if ui.selectable_label(chosen, "Automatic bitrate").clicked() {
                    settings.share_kbps = None;
                }
                for kbps in SHARE_MBPS.map(|mbps| mbps * 1000) {
                    let chosen = settings.share_kbps == Some(kbps);
                    if ui.selectable_label(chosen, set_bitrate(kbps)).clicked() {
                        settings.share_kbps = Some(kbps);
                    }
                }
            });
        ComboBox::from_id_salt("share-codec")
            .width(ui.available_width())
            .selected_text(codec.label())
            .truncate()
            .show_ui(ui, |ui| {
                for choice in codecs {
                    let chosen = codec == *choice;
                    if ui.selectable_label(chosen, choice.label()).clicked() {
                        settings.share_codec = choice.id().into();
                    }
                }
            });
    });
}

/// Starts and stops sharing our own screen, across the width available.
fn share_button(ui: &mut Ui, share: &OwnShare, actions: &mut Vec<Action>) {
    let size = Vec2::new(ui.available_width(), BIG_BUTTON_HEIGHT);
    match share {
        OwnShare::NotSharing => {
            let button = toggle(icon::MONITOR, "Share screen", false);
            if ui.add_sized(size, button).clicked() {
                actions.push(Action::ShareScreen);
            }
        }
        OwnShare::Starting => {
            let button = toggle(icon::MONITOR, "Choosing a screen…", false);
            ui.add_enabled(false, |ui: &mut Ui| ui.add_sized(size, button));
        }
        OwnShare::Sharing(_) => {
            let button = toggle(icon::MONITOR, "Stop sharing", true);
            if ui.add_sized(size, button).clicked() {
                actions.push(Action::StopSharing);
            }
        }
    }
}

/// Asks which screen or window to share, over the rest of the window.
fn share_picker(ui: &mut Ui, choices: &Choices, actions: &mut Vec<Action>) {
    let window = ui.ctx().content_rect();
    let width = (window.width() - 48.0).min(PICKER_WIDTH);
    // Everything but the list of windows has a fixed height.
    let fixed = 150.0 + CHOICE_HEIGHT * choices.screens.len() as f32;
    let room_for_windows = (window.height() - fixed).max(CHOICE_HEIGHT * 3.0);
    let frame = Frame::new()
        .fill(theme::colors().panel)
        .stroke(Stroke::new(1.0, theme::colors().border))
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::symmetric(16, 14));
    let modal = egui::Modal::new(egui::Id::new("share-picker"))
        .frame(frame)
        .show(ui.ctx(), |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 2.0;
            row(ui, theme::CONTROL_HEIGHT, |ui| {
                ui.label(strong("Share what?"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        actions.push(Action::ShareNothing);
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(muted("Screens"));
            for choice in &choices.screens {
                if choice_row(ui, icon::MONITOR, choice).clicked() {
                    actions.push(Action::Share(Picked::Screen(choice.id)));
                }
            }
            ui.add_space(8.0);
            ui.label(muted("Windows"));
            if choices.windows.is_empty() {
                ui.label(muted("No window is open."));
            }
            ScrollArea::vertical()
                .max_height(room_for_windows)
                .show(ui, |ui| {
                    for choice in &choices.windows {
                        if choice_row(ui, icon::APP_WINDOW, choice).clicked() {
                            actions.push(Action::Share(Picked::Window(choice.id)));
                        }
                    }
                });
        });
    // Escape, or a click beside the panel.
    if modal.should_close() {
        actions.push(Action::ShareNothing);
    }
}

/// One thing that could be shared: what it is on the left, and on the
/// right its size or the program it belongs to.
fn choice_row(ui: &mut Ui, glyph: &str, choice: &Choice) -> egui::Response {
    const PAD: f32 = 8.0;
    let size = Vec2::new(ui.available_width(), CHOICE_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &choice.title));
    let painter = ui.painter();
    if response.hovered() || response.has_focus() {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS),
            theme::colors().control_hover,
        );
    }
    // A line of text cut short with "…" where it would pass `width`.
    let line = |text: &str, size: f32, color: egui::Color32, width: f32| {
        let mut job =
            LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(size), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
        painter.layout_job(job)
    };
    painter.text(
        rect.left_center() + Vec2::new(PAD, 0.0),
        Align2::LEFT_CENTER,
        glyph,
        FontId::proportional(ICON_SIZE),
        theme::colors().muted,
    );
    let inner = rect.width() - 2.0 * PAD - ICON_SIZE - 8.0;
    let detail = line(
        &choice.detail,
        theme::SMALL,
        theme::colors().muted,
        inner * 0.4,
    );
    let title = line(
        &choice.title,
        theme::BODY,
        theme::colors().text,
        inner - detail.size().x - 12.0,
    );
    let title_at = egui::pos2(
        rect.left() + PAD + ICON_SIZE + 8.0,
        rect.center().y - title.size().y / 2.0,
    );
    let detail_at = egui::pos2(
        rect.right() - PAD - detail.size().x,
        rect.center().y - detail.size().y / 2.0,
    );
    painter.galley(title_at, title, theme::colors().text);
    painter.galley(detail_at, detail, theme::colors().muted);
    response
}

/// The small button that opens someone's shared screen.
fn watch_button(ui: &mut Ui, peer: &PeerRow) -> egui::Response {
    let text = strong("Watch")
        .size(theme::SMALL)
        .color(theme::colors().on_accent);
    let button = Button::new(text)
        .fill(theme::colors().accent)
        .min_size(Vec2::new(0.0, 24.0));
    let response = ui.add(button);
    response.widget_info(|| {
        let label = format!("Watch {}'s screen", peer.name);
        WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), label)
    });
    response
}

/// A button that turns orange while the state it controls is on.
fn toggle<'a>(glyph: &str, label: &str, on: bool) -> Button<'a> {
    if on {
        Button::new(icon_label(glyph, label, theme::colors().on_warn, true))
            .fill(theme::colors().warn)
    } else {
        Button::new(icon_label(glyph, label, theme::colors().text, false))
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
    state.messages_seen = message_count(room);
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
                            .color(theme::colors().muted),
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
        painter.circle_stroke(center, 3.5, Stroke::new(1.5, theme::colors().border));
    } else if peer.muted {
        painter.text(
            center,
            Align2::CENTER_CENTER,
            icon::MICROPHONE_SLASH,
            FontId::proportional(14.0),
            theme::colors().warn,
        );
    } else if peer.speaking {
        painter.circle_filled(center, 3.5, theme::colors().accent);
        painter.circle_stroke(center, 6.5, Stroke::new(1.5, theme::colors().accent));
    } else {
        painter.circle_stroke(center, 3.5, Stroke::new(1.5, theme::colors().idle));
    }
}

fn meter(ui: &mut Ui, level: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
    let radius = CornerRadius::same(3);
    ui.painter()
        .rect_filled(rect, radius, theme::colors().border);
    if level > 0.0 {
        let mut filled = rect;
        filled.set_width(rect.width() * level.clamp(0.0, 1.0));
        ui.painter()
            .rect_filled(filled, radius, theme::colors().accent);
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
        RichText::new(&peer.name).color(theme::colors().muted)
    }
}

fn status(peer: &PeerRow) -> RichText {
    let (text, color) = match peer.state {
        ConnectionState::Connecting => ("Connecting…", theme::colors().muted),
        ConnectionState::Disconnected => ("Connection lost", theme::colors().warn),
        ConnectionState::Failed => ("Unreachable", theme::colors().warn),
        ConnectionState::Closed => ("Closed", theme::colors().muted),
        ConnectionState::Connected if peer.muted => ("Muted", theme::colors().warn),
        ConnectionState::Connected if peer.speaking => ("Speaking", theme::colors().text),
        ConnectionState::Connected => ("Idle", theme::colors().muted),
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
    let people = match room.peers.len() {
        0 => return "Only you so far".into(),
        others if others == connected => format!("{total} in the room"),
        others => format!("{total} in the room, {connected} of {others} connected"),
    };
    match room.sharing_count() {
        0 => people,
        sharing => format!("{people}, {sharing} sharing a screen"),
    }
}

fn message_count(room: &RoomModel) -> usize {
    room.timeline
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Message { .. }))
        .count()
}

/// How many chat messages have arrived since the chat was last on screen.
fn unread(room: &RoomModel, state: &UiState) -> usize {
    message_count(room).saturating_sub(state.messages_seen)
}

fn own_share_text(share: &OwnShare) -> String {
    match share {
        OwnShare::NotSharing => "Not sharing".into(),
        OwnShare::Starting => "Starting…".into(),
        OwnShare::Sharing(sharing) => format!(
            "{} · {} fps",
            resolution(sharing.width, sharing.height),
            sharing.fps
        ),
    }
}

/// Names in a row, or `nobody` when there are none.
fn name_list(names: &[String], nobody: &str) -> String {
    if names.is_empty() {
        nobody.to_owned()
    } else {
        names.join(", ")
    }
}

fn resolution(width: u32, height: u32) -> String {
    if width == 0 {
        "–".into()
    } else {
        format!("{width} × {height}")
    }
}

/// A video bitrate, in the unit that suits its size.
fn bitrate(kbps: Option<f64>) -> String {
    match kbps {
        Some(kbps) if kbps >= 1000.0 => format!("{:.1} Mbps", kbps / 1000.0),
        Some(kbps) => format!("{kbps:.0} kbps"),
        None => "–".into(),
    }
}

/// What the interface shows about a screen being watched.
struct StreamFigures {
    resolution: String,
    frame_rate: String,
    /// The codec, and a note if the processor has to decode it.
    codec: String,
    bitrate: String,
    delay: String,
    frames_dropped: String,
    packets_lost: String,
    route: String,
    /// Whether sound comes with the picture, and its bitrate.
    sound: bool,
    sound_bitrate: String,
}

impl StreamFigures {
    fn of(room: &RoomModel, watching: &Watching, state: &UiState) -> Self {
        let stats = &watching.stats;
        let peer = room.peer(watching.id);
        let started = stats.width > 0;
        // From the screen being captured to the frame being ready to show:
        // encoding, half a round trip, the receive buffer and decoding.
        // What the capture itself and the display add is not known.
        let delay = peer.filter(|_| started).and_then(|peer| {
            let sharing = peer.sharing.as_ref()?;
            let network = peer.stats.round_trip_ms? / 2.0;
            let buffer = f64::from(state.settings.receive_buffer_ms);
            Some(sharing.encode_ms + network + buffer + stats.decode_ms)
        });
        Self {
            resolution: resolution(stats.width, stats.height),
            frame_rate: if started {
                format!("{:.0} fps", stats.fps)
            } else {
                "–".into()
            },
            codec: match peer.and_then(|peer| peer.sharing.as_ref()) {
                Some(sharing) if stats.software => {
                    format!("{} (software)", sharing.codec.label())
                }
                Some(sharing) => sharing.codec.label().into(),
                None => "–".into(),
            },
            bitrate: bitrate(peer.and_then(|peer| peer.stats.video_receiving_kbps)),
            delay: millis(delay),
            frames_dropped: percent(started.then_some(stats.dropped_percent)),
            packets_lost: percent(peer.and_then(|peer| peer.stats.video_loss_percent)),
            route: peer.map_or_else(|| "–".into(), route),
            sound: peer
                .and_then(|peer| peer.sharing.as_ref())
                .is_some_and(|sharing| sharing.sound),
            sound_bitrate: kbps(peer.and_then(|peer| peer.stats.sound_receiving_kbps)),
        }
    }
}

/// A figure in a table cell; dashes for missing values are dimmed.
fn figure(text: String) -> RichText {
    let color = if text == "–" {
        theme::colors().muted
    } else {
        theme::colors().text
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
        assert_eq!(bitrate(Some(8437.0)), "8.4 Mbps");
        assert_eq!(bitrate(Some(684.0)), "684 kbps");
        assert_eq!(resolution(1920, 1080), "1920 × 1080");
        assert_eq!(resolution(0, 0), "–");
        assert_eq!(clock(Duration::from_secs(42 * 60 + 17)), "00:42:17");
    }
}
