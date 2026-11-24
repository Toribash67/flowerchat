//! The flowerchat window: connects the views to a session.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use flowerchat_app::model::{AudioSummary, RoomModel};
use flowerchat_app::theme;
use flowerchat_app::views::{self, Action, Screen, UiState};
use flowerchat_client::session::{self, Command, Event, SessionConfig};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

const DEFAULT_SERVER: &str = "ws://127.0.0.1:8080/ws";
const DEFAULT_ROOM: &str = "lounge";
/// Longest the window goes without redrawing while in a room, so that
/// timers and fading indicators keep moving when no events arrive.
const REFRESH: Duration = Duration::from_millis(250);
/// How long closing the window waits for the session to wind down.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Voice chat for a small group of friends.
#[derive(Parser)]
#[command(name = "flowerchat", version)]
struct Args {
    /// Name to join with, instead of the one used last time.
    #[arg(long)]
    name: Option<String>,
    /// Room to join, instead of the one used last time.
    #[arg(long)]
    room: Option<String>,
    /// WebSocket address of the signaling server, instead of the one used
    /// last time.
    #[arg(long)]
    server: Option<String>,
    /// Join straight away instead of showing the form first.
    #[arg(long)]
    autojoin: bool,
    /// Microphone to use, as a PipeWire/PulseAudio source name.
    #[arg(long)]
    input: Option<String>,
    /// Speakers to use, as a PipeWire/PulseAudio sink name.
    #[arg(long)]
    output: Option<String>,
    /// Receive buffer in milliseconds.
    #[arg(long, default_value_t = 40)]
    jitter_ms: u32,
    /// Send the microphone without echo cancellation, noise suppression or
    /// automatic gain.
    #[arg(long)]
    raw_mic: bool,
    /// For testing: send one click per second instead of the microphone.
    #[arg(long)]
    test_tone: bool,
    /// For testing: play nothing, and print how long clicks took to arrive.
    #[arg(long)]
    measure: bool,
}

/// A session that is being joined or is under way.
struct Active {
    commands: UnboundedSender<Command>,
    task: JoinHandle<anyhow::Result<()>>,
    shared: Arc<Mutex<Shared>>,
}

/// What the window draws. A background task keeps it current, because the
/// window is not drawn, and so runs no code, while it is hidden.
struct Shared {
    room: RoomModel,
    /// Set once the server has let us in.
    joined_at: Option<Instant>,
}

struct App {
    args: Args,
    runtime: Runtime,
    ctx: egui::Context,
    ui: UiState,
    active: Option<Active>,
    /// Why the last join or session failed, shown on the join form.
    error: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: Args) -> anyhow::Result<Self> {
        theme::install(&cc.egui_ctx);
        let saved = |key: &str| cc.storage.and_then(|storage| storage.get_string(key));
        let ui = UiState {
            name: args
                .name
                .clone()
                .or_else(|| saved("name"))
                .unwrap_or_default(),
            room: args
                .room
                .clone()
                .or_else(|| saved("room"))
                .unwrap_or_else(|| DEFAULT_ROOM.into()),
            server: args
                .server
                .clone()
                .or_else(|| saved("server"))
                .unwrap_or_else(|| DEFAULT_SERVER.into()),
            ..UiState::default()
        };
        let mut app = Self {
            runtime: Runtime::new()?,
            ctx: cc.egui_ctx.clone(),
            ui,
            active: None,
            error: None,
            args,
        };
        if app.args.autojoin {
            app.perform(Action::Join);
        }
        Ok(app)
    }

    /// Notices when the session has ended, and why.
    fn reap(&mut self) {
        let finished = self
            .active
            .as_ref()
            .is_some_and(|active| active.task.is_finished());
        if finished && let Some(active) = self.active.take() {
            self.error = match self.runtime.block_on(active.task) {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(sentence(&format!("{err:#}"))),
                Err(_) => Some("The session stopped unexpectedly.".into()),
            };
        }
    }

    fn perform(&mut self, action: Action) {
        if action == Action::Join {
            self.join();
            return;
        }
        let Some(active) = &self.active else {
            return;
        };
        let room = &mut active.shared.lock().unwrap().room;
        let command = match action {
            Action::Join => return,
            Action::Leave => Command::Leave,
            Action::SetMuted(muted) => {
                room.me.muted = muted;
                Command::SetMuted(muted)
            }
            Action::SetDeafened(deafened) => {
                room.me.deafened = deafened;
                Command::SetDeafened(deafened)
            }
            Action::SetPeerVolume { id, volume } => {
                if let Some(peer) = room.peers.iter_mut().find(|peer| peer.id == id) {
                    peer.volume = volume;
                }
                Command::SetPeerVolume {
                    id,
                    volume: f64::from(volume),
                }
            }
            Action::SendChat(text) => {
                let me = room.me.name.clone();
                room.say(&clock(), me, text.clone());
                Command::SendChat(text)
            }
        };
        let _ = active.commands.send(command);
    }

    fn join(&mut self) {
        if self.active.is_some() {
            return;
        }
        self.error = None;
        let (name, room, server) = (
            self.ui.name.trim().to_owned(),
            self.ui.room.trim().to_owned(),
            self.ui.server.trim().to_owned(),
        );
        let config = SessionConfig {
            server: server.clone(),
            room: room.clone(),
            name: name.clone(),
            input: self.args.input.clone(),
            output: self.args.output.clone(),
            jitter_ms: self.args.jitter_ms,
            raw_mic: self.args.raw_mic,
            test_tone: self.args.test_tone,
            measure: self.args.measure,
        };
        let device = |name: &Option<String>| name.clone().unwrap_or("System default".into());
        let audio = AudioSummary {
            microphone: if self.args.test_tone {
                "Test clicks".into()
            } else {
                device(&self.args.input)
            },
            speakers: if self.args.measure {
                "None (measuring)".into()
            } else {
                device(&self.args.output)
            },
            processing: !self.args.raw_mic && !self.args.test_tone,
            receive_buffer_ms: self.args.jitter_ms,
        };

        let (commands, command_rx) = unbounded_channel();
        let (event_tx, events) = unbounded_channel();
        let shared = Arc::new(Mutex::new(Shared {
            room: RoomModel::new(&room, host_of(&server), &name, audio),
            joined_at: None,
        }));
        self.runtime
            .spawn(follow(events, shared.clone(), self.ctx.clone()));
        self.active = Some(Active {
            commands,
            task: self
                .runtime
                .spawn(session::run(config, command_rx, event_tx)),
            shared,
        });
    }
}

/// Keeps the model up to date with the session until the session ends.
async fn follow(
    mut events: UnboundedReceiver<Event>,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    while let Some(event) = events.recv().await {
        let now = Instant::now();
        {
            let mut shared = shared.lock().unwrap();
            match &event {
                Event::Joined { .. } => shared.joined_at = Some(now),
                Event::ClickDelay(delay_ns) => {
                    println!("click delay {:.1} ms", *delay_ns as f64 / 1e6);
                }
                _ => {}
            }
            shared.room.apply(event, now, &clock());
        }
        ctx.request_repaint();
    }
    // The session is over; the window has to redraw to notice.
    ctx.request_repaint();
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.reap();
        let actions = match &self.active {
            Some(active) => {
                let mut shared = active.shared.lock().unwrap();
                let screen = match shared.joined_at {
                    Some(joined_at) => {
                        shared.room.tick(Instant::now(), joined_at);
                        Screen::Room(&shared.room)
                    }
                    None => Screen::Lobby {
                        connecting: true,
                        error: None,
                    },
                };
                views::draw(ui, screen, &mut self.ui)
            }
            None => {
                let screen = Screen::Lobby {
                    connecting: false,
                    error: self.error.as_deref(),
                };
                views::draw(ui, screen, &mut self.ui)
            }
        };
        for action in actions {
            self.perform(action);
        }
        if self.active.is_some() {
            ui.ctx().request_repaint_after(REFRESH);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("name", self.ui.name.clone());
        storage.set_string("room", self.ui.room.clone());
        storage.set_string("server", self.ui.server.clone());
    }

    fn on_exit(&mut self) {
        // Leave properly, so the others see us go and audio devices close.
        if let Some(active) = self.active.take() {
            let _ = active.commands.send(Command::Leave);
            let _ = self
                .runtime
                .block_on(tokio::time::timeout(EXIT_GRACE, active.task));
        }
    }
}

/// The current wall-clock time as `HH:MM`.
fn clock() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// The host part of a server address, for display.
fn host_of(server: &str) -> &str {
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Turns an error chain into something that reads as a sentence.
fn sentence(error: &str) -> String {
    let mut chars = error.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut text: String = first.to_uppercase().chain(chars).collect();
    if !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    gstreamer::init()?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("flowerchat")
            .with_app_id("flowerchat")
            .with_inner_size([1280.0, 720.0])
            .with_min_inner_size([340.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "flowerchat",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, args)?))),
    )
    .map_err(|err| anyhow::anyhow!("cannot open the window: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_cut_out_of_the_address() {
        assert_eq!(host_of("wss://chat.example.org/ws"), "chat.example.org");
        assert_eq!(host_of("ws://127.0.0.1:8080/ws"), "127.0.0.1:8080");
        assert_eq!(host_of("example.org"), "example.org");
    }

    #[test]
    fn errors_become_sentences() {
        assert_eq!(
            sentence("cannot reach x: refused"),
            "Cannot reach x: refused."
        );
        assert_eq!(sentence("Already fine."), "Already fine.");
    }
}
