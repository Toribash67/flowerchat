//! The flowerchat window: connects the views to a session.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use flowerchat_app::model::{RoomModel, meter_level};
use flowerchat_app::settings::{DeviceChoice, SendMode, Settings};
use flowerchat_app::talk::Talk;
use flowerchat_app::theme;
use flowerchat_app::views::{self, Action, Env, Screen, UiState};
use flowerchat_client::audio::{self, AudioDevice, AudioDevices, DeviceWatch, MicCheck};
use flowerchat_client::session::{self, AudioSettings, Command, Event, SessionConfig};
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
    /// Microphone to use, as a PipeWire/PulseAudio source name, instead of
    /// the one chosen last time.
    #[arg(long)]
    input: Option<String>,
    /// Speakers to use, as a PipeWire/PulseAudio sink name, instead of the
    /// ones chosen last time.
    #[arg(long)]
    output: Option<String>,
    /// Receive buffer in milliseconds, instead of the one chosen last time.
    #[arg(long)]
    jitter_ms: Option<u32>,
    /// Turn off echo cancellation, noise suppression and automatic volume.
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
    /// The audio settings the session was last given.
    audio: AudioSettings,
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
    /// Absent when this system cannot list its audio devices.
    watch: Option<DeviceWatch>,
    devices: AudioDevices,
    /// Listens to the microphone while not in a room, so the join screen
    /// can show that it works. Holds the settings it was started with, and
    /// nothing more if it could not start.
    mic_check: Option<(AudioSettings, Option<MicCheck>)>,
    /// The level the microphone check last reported, as the bits of an
    /// `f32` from 0 to 1.
    lobby_level: Arc<AtomicU32>,
    talk: Arc<Talk>,
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
            settings: settings_with(
                cc.storage
                    .and_then(|storage| eframe::get_value(storage, "settings"))
                    .unwrap_or_default(),
                &args,
            ),
            ..UiState::default()
        };
        let watch = DeviceWatch::start()
            .inspect_err(|err| tracing::warn!("audio devices cannot be chosen: {err:#}"))
            .ok();
        let mut app = Self {
            runtime: Runtime::new()?,
            ctx: cc.egui_ctx.clone(),
            ui,
            active: None,
            error: None,
            devices: AudioDevices::default(),
            watch,
            mic_check: None,
            lobby_level: Arc::default(),
            talk: Arc::default(),
            args,
        };
        app.list_devices();
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
            self.talk.detach();
            self.error = match self.runtime.block_on(active.task) {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(sentence(&format!("{err:#}"))),
                Err(_) => Some("The session stopped unexpectedly.".into()),
            };
        }
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Join => return self.join(),
            Action::TestSpeakers => return self.test_speakers(),
            _ => {}
        }
        let Some(active) = &self.active else {
            return;
        };
        let room = &mut active.shared.lock().unwrap().room;
        let command = match action {
            Action::Join | Action::TestSpeakers => return,
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

    /// The audio settings to use, given what is chosen and plugged in.
    fn audio(&self) -> AudioSettings {
        self.ui.settings.audio(&self.devices)
    }

    /// Looks up which audio devices are plugged in.
    fn list_devices(&mut self) {
        let Some(watch) = &self.watch else {
            return;
        };
        self.devices = watch.devices();
        // A device named on the command line can be chosen even when the
        // system does not list it, as is the case for monitors of outputs.
        for (flag, list) in [
            (&self.args.input, &mut self.devices.inputs),
            (&self.args.output, &mut self.devices.outputs),
        ] {
            if let Some(name) = flag
                && !list.iter().any(|device| device.id == *name)
            {
                list.push(AudioDevice {
                    id: name.clone(),
                    label: name.clone(),
                    is_default: false,
                });
            }
        }
    }

    fn test_speakers(&mut self) {
        let output = self.audio().output;
        let Err(err) = audio::play_test_sound(output.as_deref()) else {
            return;
        };
        let problem = sentence(&format!("{err:#}"));
        match &self.active {
            Some(active) => active.shared.lock().unwrap().room.log(&clock(), problem),
            None => self.error = Some(problem),
        }
    }

    /// Brings the session in line with the audio settings, and decides
    /// whether the microphone is let out.
    fn sync_audio(&mut self, ctx: &egui::Context) {
        if self.watch.as_ref().is_some_and(DeviceWatch::changed) {
            self.list_devices();
        }

        let push_to_talk = self.ui.settings.send == SendMode::WhileKeyHeld;
        if push_to_talk {
            let ctx = ctx.clone();
            self.talk
                .register_global(&self.runtime, move || ctx.request_repaint());
        }
        self.talk
            .set_local(push_to_talk, views::talk_key_down(ctx, &self.ui));

        let audio = self.audio();
        match &mut self.active {
            Some(active) => {
                self.mic_check = None;
                if active.audio != audio {
                    active.audio = audio.clone();
                    let _ = active.commands.send(Command::SetAudio(audio));
                }
            }
            None => self.check_microphone(audio),
        }
    }

    /// Keeps a microphone check running with the settings given.
    fn check_microphone(&mut self, audio: AudioSettings) {
        if self.args.test_tone {
            return;
        }
        // The check plays nothing, so only the capturing side matters.
        let wanted = AudioSettings {
            output: None,
            jitter_ms: 0,
            ..audio
        };
        if self
            .mic_check
            .as_ref()
            .is_some_and(|(running, _)| *running == wanted)
        {
            return;
        }
        // The old check has to let go of the microphone first.
        self.mic_check = None;
        self.lobby_level.store(0, Ordering::Relaxed);
        let (level, ctx) = (self.lobby_level.clone(), self.ctx.clone());
        let check = MicCheck::start(&wanted, move |db| {
            level.store(meter_level(db).to_bits(), Ordering::Relaxed);
            ctx.request_repaint();
        })
        .inspect_err(|err| tracing::warn!("cannot check the microphone: {err:#}"))
        .ok();
        self.mic_check = Some((wanted, check));
    }

    fn join(&mut self) {
        if self.active.is_some() {
            return;
        }
        self.error = None;
        // The call needs the microphone that the check is holding.
        self.mic_check = None;
        let (name, room, server) = (
            self.ui.name.trim().to_owned(),
            self.ui.room.trim().to_owned(),
            self.ui.server.trim().to_owned(),
        );
        let audio = self.audio();
        let transmitting = self.talk.transmitting();
        let config = SessionConfig {
            server: server.clone(),
            room: room.clone(),
            name: name.clone(),
            audio: audio.clone(),
            transmitting,
            test_tone: self.args.test_tone,
            measure: self.args.measure,
        };

        let (commands, command_rx) = unbounded_channel();
        let (event_tx, events) = unbounded_channel();
        let shared = Arc::new(Mutex::new(Shared {
            room: RoomModel::new(&room, host_of(&server), &name),
            joined_at: None,
        }));
        self.runtime
            .spawn(follow(events, shared.clone(), self.ctx.clone()));
        self.talk.attach(commands.clone(), transmitting);
        self.active = Some(Active {
            commands,
            task: self
                .runtime
                .spawn(session::run(config, command_rx, event_tx)),
            shared,
            audio,
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
        self.sync_audio(ui.ctx());
        let env = Env {
            devices: &self.devices,
            global_key: self.talk.global_key(),
            transmitting: self.talk.transmitting(),
        };
        let mic_level = f32::from_bits(self.lobby_level.load(Ordering::Relaxed));
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
                        mic_level: 0.0,
                    },
                };
                views::draw(ui, screen, &env, &mut self.ui)
            }
            None => {
                let screen = Screen::Lobby {
                    connecting: false,
                    error: self.error.as_deref(),
                    mic_level,
                };
                views::draw(ui, screen, &env, &mut self.ui)
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
        eframe::set_value(storage, "settings", &self.ui.settings);
    }

    fn on_exit(&mut self) {
        // Leave properly, so the others see us go and audio devices close.
        self.mic_check = None;
        if let Some(active) = self.active.take() {
            let _ = active.commands.send(Command::Leave);
            let _ = self
                .runtime
                .block_on(tokio::time::timeout(EXIT_GRACE, active.task));
        }
    }
}

/// The saved settings with the command line's choices on top.
fn settings_with(mut settings: Settings, args: &Args) -> Settings {
    let named = |name: &String| DeviceChoice {
        id: name.clone(),
        label: name.clone(),
    };
    if let Some(input) = &args.input {
        settings.microphone = Some(named(input));
    }
    if let Some(output) = &args.output {
        settings.speakers = Some(named(output));
    }
    if let Some(jitter_ms) = args.jitter_ms {
        settings.receive_buffer_ms = jitter_ms;
    }
    if args.raw_mic {
        settings.echo_cancellation = false;
        settings.noise_suppression = false;
        settings.automatic_volume = false;
    }
    settings
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
mod e2e;

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
