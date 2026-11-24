//! The flowerchat window: connects the views to a session.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use flowerchat_app::model::{OwnShare, RoomModel, meter_level};
use flowerchat_app::screen::{self, Cast, Pick};
use flowerchat_app::settings::{DeviceChoice, SendMode, Settings};
use flowerchat_app::talk::Talk;
use flowerchat_app::theme;
use flowerchat_app::views::{self, Action, Env, Picture, Screen, UiState};
use flowerchat_client::audio::{self, AudioDevice, AudioDevices, DeviceWatch, MicCheck};
use flowerchat_client::session::{
    self, AudioSettings, Codec, Command, Event, FrameSink, SessionConfig, ShareSettings,
    ShareSource, VideoFrame,
};
use flowerchat_client::video::parse_size;
use flowerchat_protocol::PeerId;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

const DEFAULT_SERVER: &str = "wss://chat.flowerboi.se/ws";
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
    /// Scale a shared screen down to this height if it is taller, instead
    /// of what was chosen last time. 0 sends it at the size it has.
    #[arg(long)]
    share_height: Option<u32>,
    /// Frames per second to share a screen at, instead of what was chosen
    /// last time.
    #[arg(long)]
    share_fps: Option<u32>,
    /// For testing: share a moving test picture of this size, such as
    /// 1920x1080, instead of asking which screen to share.
    #[arg(long, value_name = "WIDTHxHEIGHT", value_parser = size)]
    share_test: Option<(u32, u32)>,
    /// For testing: lose this many percent of the packets that arrive, as
    /// a poor network would.
    #[arg(long, default_value_t = 0.0)]
    lose: f64,
}

fn size(text: &str) -> Result<(u32, u32), String> {
    parse_size(text).ok_or_else(|| "not a size like 1920x1080".to_owned())
}

/// A session that is being joined or is under way.
struct Active {
    commands: UnboundedSender<Command>,
    task: JoinHandle<anyhow::Result<()>>,
    shared: Arc<Mutex<Shared>>,
    /// The audio settings the session was last given.
    audio: AudioSettings,
    /// The newest frame of a watched screen that has not been drawn yet,
    /// and whose screen it is.
    frame: Arc<Mutex<Option<(PeerId, VideoFrame)>>>,
    /// The desktop's permission to read our screen, while we share it.
    cast: Arc<Mutex<Option<Cast>>>,
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
    /// The screen being watched, as far as it has arrived.
    picture: Option<egui::TextureHandle>,
    /// Whose screen the session was last told a size for, and the size.
    view_height: Option<(PeerId, u32)>,
    /// The size and rate the session was last told to share at.
    share_settings: Option<ShareSettings>,
    /// The codecs this machine can share a screen in.
    codecs: Vec<Codec>,
}

/// The heights at which frames of a watched screen are asked for. The
/// smallest that is no smaller than the picture on the display is used, so
/// that resizing the window does not mean a new size every moment.
const VIEW_HEIGHTS: [u32; 9] = [360, 540, 720, 900, 1080, 1260, 1440, 1800, 2160];

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
            picture: None,
            view_height: None,
            share_settings: None,
            codecs: Codec::ALL
                .into_iter()
                .filter(|codec| codec.can_encode())
                .collect(),
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
            Action::ShareScreen => return self.share_screen(),
            _ => {}
        }
        let Some(active) = &self.active else {
            return;
        };
        // Giving back the permission to read the screen needs the runtime.
        let _runtime = self.runtime.enter();
        let room = &mut active.shared.lock().unwrap().room;
        let command = match action {
            Action::Join | Action::TestSpeakers | Action::ShareScreen => return,
            Action::StopSharing => {
                active.cast.lock().unwrap().take();
                Command::StopShare
            }
            Action::Watch(id) => {
                if let Some(watching) = &room.watching {
                    if watching.id == id {
                        return;
                    }
                    let previous = watching.id;
                    room.stop_watching(&clock());
                    let _ = active.commands.send(Command::Watch {
                        id: previous,
                        watch: false,
                    });
                }
                room.watch(id, &clock());
                // What is left of the last screen is not this one.
                self.picture = None;
                self.view_height = None;
                Command::Watch { id, watch: true }
            }
            Action::StopWatching => {
                let Some(id) = room.watching.as_ref().map(|watching| watching.id) else {
                    return;
                };
                room.stop_watching(&clock());
                Command::Watch { id, watch: false }
            }
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
            Action::SetStreamVolume { id, volume } => {
                if let Some(peer) = room.peers.iter_mut().find(|peer| peer.id == id) {
                    peer.stream_volume = volume;
                }
                Command::SetStreamVolume {
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

    /// Asks which screen to share, and shares it.
    fn share_screen(&mut self) {
        let Some(active) = &self.active else {
            return;
        };
        let settings = self.ui.settings.share();
        self.share_settings = Some(settings);
        active.shared.lock().unwrap().room.me.share = OwnShare::Starting;
        if let Some((width, height)) = self.args.share_test {
            let source = ShareSource::TestPicture { width, height };
            let _ = active
                .commands
                .send(Command::StartShare { source, settings });
            return;
        }

        let (commands, shared) = (active.commands.clone(), active.shared.clone());
        let (cast_slot, ctx) = (active.cast.clone(), self.ctx.clone());
        self.runtime.spawn(async move {
            let outcome = screen::pick().await;
            let mut shared = shared.lock().unwrap();
            match outcome {
                Ok(Pick::Chosen { source, cast }) => {
                    *cast_slot.lock().unwrap() = Some(cast);
                    let source = ShareSource::Screen(source);
                    let _ = commands.send(Command::StartShare { source, settings });
                }
                Ok(Pick::Cancelled) => shared.room.me.share = OwnShare::NotSharing,
                Err(reason) => {
                    shared.room.me.share = OwnShare::NotSharing;
                    let problem = format!("the screen cannot be shared, because {reason}");
                    shared.room.log(&clock(), sentence(&problem));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Brings the picture of the watched screen up to date, and says what
    /// there is to draw.
    fn update_picture(&mut self, ctx: &egui::Context) -> Option<Picture> {
        let watched = self.active.as_ref().and_then(|active| {
            let shared = active.shared.lock().unwrap();
            shared.room.watching.as_ref().map(|watching| watching.id)
        });
        let frame = self
            .active
            .as_ref()
            .and_then(|active| active.frame.lock().unwrap().take());
        let Some(watched) = watched else {
            self.picture = None;
            return None;
        };
        if let Some((from, frame)) = frame
            && from == watched
        {
            let size = [frame.width as usize, frame.height as usize];
            // Frames have no transparency, so their colours are already
            // what egui calls premultiplied.
            let image = egui::ColorImage::from_rgba_premultiplied(size, frame.rgba());
            let options = egui::TextureOptions::LINEAR;
            match &mut self.picture {
                Some(texture) => texture.set(image, options),
                None => self.picture = Some(ctx.load_texture("shared-screen", image, options)),
            }
        }
        self.picture.as_ref().map(|texture| Picture {
            texture: texture.id(),
            size: texture.size_vec2(),
        })
    }

    /// Brings a share under way in line with the size and rate chosen.
    fn sync_share(&mut self) {
        let Some(active) = &self.active else {
            return;
        };
        let sharing = matches!(
            active.shared.lock().unwrap().room.me.share,
            OwnShare::Sharing(_)
        );
        let settings = self.ui.settings.share();
        if sharing && self.share_settings != Some(settings) {
            self.share_settings = Some(settings);
            let _ = active.commands.send(Command::SetShareSettings(settings));
        }
    }

    /// Asks for the frames of the watched screen at about the size they are
    /// drawn at. Larger ones would cost time to prepare, for nothing.
    fn sync_view_height(&mut self) {
        let Some(active) = &self.active else {
            self.view_height = None;
            return;
        };
        let watched = {
            let shared = active.shared.lock().unwrap();
            shared.room.watching.as_ref().map(|watching| watching.id)
        };
        let (Some(id), Some(drawn)) = (watched, self.ui.picture_height) else {
            return;
        };
        // Past the largest step, frames come at whatever size is sent.
        let max_height = VIEW_HEIGHTS
            .into_iter()
            .find(|height| *height >= drawn)
            .unwrap_or(0);
        if self.view_height != Some((id, max_height)) {
            self.view_height = Some((id, max_height));
            let _ = active
                .commands
                .send(Command::SetViewHeight { id, max_height });
        }
    }

    /// Shows the watched screen in a window of its own, for as long as the
    /// user wants it there.
    fn popped_out_window(&mut self, ctx: &egui::Context, picture: Option<Picture>) {
        let name = self.active.as_ref().and_then(|active| {
            let shared = active.shared.lock().unwrap();
            let watching = shared.room.watching.as_ref()?;
            Some(shared.room.peer(watching.id)?.name.clone())
        });
        let Some(name) = name else {
            self.ui.popped_out = false;
            return;
        };
        let viewport = egui::ViewportBuilder::default()
            .with_title(format!("{name}'s screen · flowerchat"))
            .with_app_id("flowerchat")
            .with_inner_size([1280.0, 720.0]);
        let id = egui::ViewportId::from_hash_of("shared-screen");
        let closed = ctx.show_viewport_immediate(id, viewport, |ui, _| {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::STAGE))
                .show(ui, |ui| {
                    if let Some(picture) = picture {
                        let height = views::draw_picture(ui, ui.max_rect(), picture);
                        self.ui.picture_height = Some(height);
                    }
                });
            ui.input(|input| input.viewport().close_requested())
        });
        if closed {
            self.ui.popped_out = false;
        }
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
        // Only the newest frame is kept: the window draws when it can, and
        // has no use for frames it was too late for.
        let frame: Arc<Mutex<Option<(PeerId, VideoFrame)>>> = Arc::default();
        let (slot, ctx) = (frame.clone(), self.ctx.clone());
        let frames: FrameSink = Arc::new(move |from, frame| {
            *slot.lock().unwrap() = Some((from, frame));
            ctx.request_repaint();
        });
        let config = SessionConfig {
            server: server.clone(),
            room: room.clone(),
            name: name.clone(),
            audio: audio.clone(),
            transmitting,
            test_tone: self.args.test_tone,
            measure: self.args.measure,
            frames: Some(frames),
            incoming_loss: self.args.lose / 100.0,
        };

        let (commands, command_rx) = unbounded_channel();
        let (event_tx, events) = unbounded_channel();
        let shared = Arc::new(Mutex::new(Shared {
            room: RoomModel::new(&room, host_of(&server), &name),
            joined_at: None,
        }));
        let cast: Arc<Mutex<Option<Cast>>> = Arc::default();
        self.runtime.spawn(follow(
            events,
            shared.clone(),
            cast.clone(),
            self.ctx.clone(),
        ));
        self.talk.attach(commands.clone(), transmitting);
        self.active = Some(Active {
            commands,
            task: self
                .runtime
                .spawn(session::run(config, command_rx, event_tx)),
            shared,
            audio,
            frame,
            cast,
        });
    }
}

/// Keeps the model up to date with the session until the session ends.
async fn follow(
    mut events: UnboundedReceiver<Event>,
    shared: Arc<Mutex<Shared>>,
    cast: Arc<Mutex<Option<Cast>>>,
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
                // However a share ends, the screen is no longer ours to
                // read.
                Event::OwnShare(None) | Event::ShareFailed(_) => {
                    cast.lock().unwrap().take();
                }
                _ => {}
            }
            shared.room.apply(event, now, &clock());
        }
        ctx.request_repaint();
    }
    // The session is over, and with it any share.
    cast.lock().unwrap().take();
    // The window has to redraw to notice.
    ctx.request_repaint();
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.reap();
        self.sync_audio(ui.ctx());
        let picture = self.update_picture(ui.ctx());
        if self.ui.popped_out {
            self.popped_out_window(ui.ctx(), picture);
        }
        let env = Env {
            devices: &self.devices,
            global_key: self.talk.global_key(),
            transmitting: self.talk.transmitting(),
            picture,
            codecs: &self.codecs,
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
        self.sync_view_height();
        self.sync_share();
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
    if let Some(height) = args.share_height {
        settings.share_height = (height > 0).then_some(height);
    }
    if let Some(fps) = args.share_fps {
        settings.share_fps = fps;
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
