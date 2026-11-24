//! Decides whether the microphone is let out, and tells the session.
//!
//! With an open mic that is always. With push-to-talk it is while a key is
//! held: the key chosen in the window, which only reaches us while the
//! window has focus, or a system-wide shortcut, which reaches us anywhere.
//! The system-wide one has to work while the window is hidden and nothing
//! is being drawn, so changes go to the session from here and not by way of
//! the interface.

use std::sync::{Arc, Mutex};

use flowerchat_client::session::Command;
use tokio::sync::mpsc::UnboundedSender;

/// The name the system-wide shortcut is registered under.
pub const SHORTCUT_ID: &str = "push-to-talk";
/// What the desktop knows the app as. Desktops only hand out system-wide
/// shortcuts to apps they can identify, which takes a launcher entry
/// (`se.flowerboi.flowerchat.desktop`) installed under this name.
pub const APP_ID: &str = "se.flowerboi.flowerchat";

/// Where the system-wide shortcut stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GlobalKey {
    /// Not asked for, because push-to-talk has not been used.
    #[default]
    Off,
    Registering,
    /// Registered. `trigger` is the key the system has bound it to, in the
    /// system's words, and empty when it does not say.
    Ready {
        trigger: String,
    },
    /// There is none, for the reason given, which completes the sentence
    /// "There is no system-wide shortcut, because ...".
    Unavailable(String),
    /// None is needed: the key chosen in the window is followed wherever
    /// it is pressed.
    SameKey,
}

#[derive(Default)]
pub struct Talk {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    push_to_talk: bool,
    window_key: bool,
    global_key: bool,
    global: GlobalKey,
    /// The key chosen in the window, and whether the window has focus.
    #[cfg_attr(not(windows), allow(dead_code))]
    key: Option<egui::Key>,
    #[cfg_attr(not(windows), allow(dead_code))]
    focused: bool,
    /// The session under way and what it was last told.
    session: Option<(UnboundedSender<Command>, bool)>,
}

impl State {
    fn transmitting(&self) -> bool {
        !self.push_to_talk || self.window_key || self.global_key
    }

    /// Tells the session, if what it was told last no longer holds.
    fn sync(&mut self) {
        let now = self.transmitting();
        if let Some((commands, told)) = &mut self.session
            && *told != now
        {
            *told = now;
            let _ = commands.send(Command::SetTransmitting(now));
        }
    }
}

impl Talk {
    pub fn transmitting(&self) -> bool {
        self.state.lock().unwrap().transmitting()
    }

    pub fn global_key(&self) -> GlobalKey {
        self.state.lock().unwrap().global.clone()
    }

    /// Sets the mode and whether the key chosen in the window is down.
    pub fn set_local(&self, push_to_talk: bool, window_key: bool) {
        let mut state = self.state.lock().unwrap();
        state.push_to_talk = push_to_talk;
        state.window_key = window_key;
        state.sync();
    }

    /// Sets the key chosen in the window and whether the window has focus,
    /// for systems where that key is followed outside the window too.
    pub fn set_window(&self, key: Option<egui::Key>, focused: bool) {
        let mut state = self.state.lock().unwrap();
        state.key = key;
        state.focused = focused;
    }

    /// Follows a new session, which was started with `transmitting`.
    pub fn attach(&self, commands: UnboundedSender<Command>, transmitting: bool) {
        let mut state = self.state.lock().unwrap();
        state.session = Some((commands, transmitting));
        state.sync();
    }

    pub fn detach(&self) {
        self.state.lock().unwrap().session = None;
    }

    fn set_global_key(&self, down: bool) {
        let mut state = self.state.lock().unwrap();
        state.global_key = down;
        state.sync();
    }

    fn set_global(&self, global: GlobalKey) {
        let mut state = self.state.lock().unwrap();
        if !matches!(global, GlobalKey::Ready { .. } | GlobalKey::SameKey) {
            // Without a shortcut nothing will ever release the key.
            state.global_key = false;
        }
        state.global = global;
        state.sync();
    }

    /// Registers the system-wide shortcut, once. `changed` is called
    /// whenever there is something new to show.
    pub fn register_global(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Runtime,
        changed: impl Fn() + Send + Sync + 'static,
    ) {
        {
            let mut state = self.state.lock().unwrap();
            if state.global != GlobalKey::Off {
                return;
            }
            state.global = GlobalKey::Registering;
        }
        let talk = self.clone();
        runtime.spawn(async move {
            let reason = match portal::listen(&talk, &changed).await {
                Ok(()) => "the desktop stopped offering it".to_owned(),
                Err(reason) => reason,
            };
            tracing::warn!("no system-wide push-to-talk shortcut, because {reason}");
            talk.set_global(GlobalKey::Unavailable(reason));
            changed();
        });
    }
}

#[cfg(target_os = "linux")]
mod portal {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;

    use super::{APP_ID, GlobalKey, SHORTCUT_ID, Talk};

    /// Registers the shortcut with the desktop and follows it for as long
    /// as the desktop keeps the registration alive. An error says why there
    /// is no shortcut, in words that can be shown.
    pub async fn listen(talk: &Talk, changed: &(impl Fn() + Sync)) -> Result<(), String> {
        follow(talk, changed).await.map_err(|err| {
            let text = err.to_string();
            if text.contains("app id") {
                format!("the desktop does not know this app (is {APP_ID}.desktop installed?)")
            } else {
                format!("the desktop refused ({text})")
            }
        })
    }

    async fn follow(talk: &Talk, changed: &(impl Fn() + Sync)) -> ashpd::Result<()> {
        // Started from a launcher, the desktop already knows who we are.
        // Started from a terminal, it has to be told, and it only believes
        // it if a launcher entry with this name is installed.
        if let Ok(id) = ashpd::AppID::try_from(APP_ID)
            && let Err(err) = ashpd::register_host_app(id).await
        {
            tracing::debug!("the desktop did not accept the name {APP_ID}: {err}");
        }
        let shortcuts = GlobalShortcuts::new().await?;
        let session = shortcuts.create_session(Default::default()).await?;
        let wanted = NewShortcut::new(SHORTCUT_ID, "Send voice while held");
        let bound = shortcuts
            .bind_shortcuts(&session, &[wanted], None, Default::default())
            .await?
            .response()?;
        let trigger = bound
            .shortcuts()
            .iter()
            .find(|shortcut| shortcut.id() == SHORTCUT_ID)
            .map(|shortcut| shortcut.trigger_description().to_owned())
            .unwrap_or_default();
        talk.set_global(GlobalKey::Ready { trigger });
        changed();

        let mut pressed = shortcuts.receive_activated().await?;
        let mut released = shortcuts.receive_deactivated().await?;
        loop {
            let down = tokio::select! {
                Some(event) = pressed.next() => (event.shortcut_id() == SHORTCUT_ID).then_some(true),
                Some(event) = released.next() => (event.shortcut_id() == SHORTCUT_ID).then_some(false),
                else => break,
            };
            if let Some(down) = down {
                tracing::debug!(down, "system-wide push-to-talk key");
                talk.set_global_key(down);
                changed();
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod portal {
    use std::time::Duration;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    use super::{GlobalKey, Talk};

    /// How often the key is looked at. A press is at most this late.
    const LOOK_EVERY: Duration = Duration::from_millis(10);

    /// Follows the key chosen in the window while the window is in the
    /// background. Windows tells any program whether a key is down, so
    /// there is nothing to register and nothing that ends this.
    pub async fn listen(talk: &Talk, changed: &(impl Fn() + Sync)) -> Result<(), String> {
        let mut look = tokio::time::interval(LOOK_EVERY);
        let mut held = false;
        loop {
            look.tick().await;
            let (key, focused) = {
                let state = talk.state.lock().unwrap();
                (state.key, state.focused)
            };
            let code = key.map(virtual_key);
            let global = match code {
                Some(None) => GlobalKey::Unavailable(
                    "this key can only be followed here; letters, digits and 
                     function keys work everywhere"
                        .to_owned(),
                ),
                _ => GlobalKey::SameKey,
            };
            if talk.global_key() != global {
                talk.set_global(global);
                changed();
            }
            // With focus, the window sees the key itself, and knows when
            // it is being typed into a text field.
            let down = !focused
                && code
                    .flatten()
                    // The top bit says whether the key is down right now.
                    .is_some_and(|code| unsafe { GetAsyncKeyState(code) } < 0);
            if down != held {
                held = down;
                tracing::debug!(down, "push-to-talk key outside the window");
                talk.set_global_key(down);
                changed();
            }
        }
    }

    /// What Windows calls a key, for the keys it calls the same on every
    /// keyboard layout.
    pub fn virtual_key(key: egui::Key) -> Option<i32> {
        use egui::Key;
        let code = match key {
            Key::Backspace => 0x08,
            Key::Tab => 0x09,
            Key::Enter => 0x0D,
            Key::Space => 0x20,
            Key::PageUp => 0x21,
            Key::PageDown => 0x22,
            Key::End => 0x23,
            Key::Home => 0x24,
            Key::ArrowLeft => 0x25,
            Key::ArrowUp => 0x26,
            Key::ArrowRight => 0x27,
            Key::ArrowDown => 0x28,
            Key::Insert => 0x2D,
            Key::Delete => 0x2E,
            _ => {
                let name = key.name();
                match name.as_bytes() {
                    // Letters and digits have the codes of their characters.
                    [character] if character.is_ascii_uppercase() || character.is_ascii_digit() => {
                        i32::from(*character)
                    }
                    [b'F', number @ ..] => {
                        let number: i32 = std::str::from_utf8(number).ok()?.parse().ok()?;
                        if !(1..=24).contains(&number) {
                            return None;
                        }
                        0x70 + number - 1
                    }
                    _ => return None,
                }
            }
        };
        Some(code)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod portal {
    use super::Talk;

    pub async fn listen(_: &Talk, _: &(impl Fn() + Sync)) -> Result<(), String> {
        Err("flowerchat cannot register one on this system yet".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    fn told(commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<bool> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let Command::SetTransmitting(on) = command {
                seen.push(on);
            }
        }
        seen
    }

    #[test]
    fn an_open_mic_always_transmits() {
        let talk = Talk::default();
        assert!(talk.transmitting());
        talk.set_local(false, false);
        assert!(talk.transmitting());
    }

    #[test]
    fn push_to_talk_follows_either_key() {
        let talk = Talk::default();
        let (commands, mut received) = unbounded_channel();
        talk.set_local(true, false);
        assert!(!talk.transmitting());
        talk.attach(commands, false);

        talk.set_local(true, true);
        talk.set_global_key(true);
        talk.set_local(true, false);
        assert!(talk.transmitting(), "the system-wide key is still down");
        talk.set_global_key(false);
        assert_eq!(told(&mut received), [true, false]);
    }

    #[test]
    fn a_session_started_out_of_step_is_corrected() {
        let talk = Talk::default();
        let (commands, mut received) = unbounded_channel();
        talk.set_local(true, false);
        talk.attach(commands, true);
        assert_eq!(told(&mut received), [false]);
    }

    #[cfg(windows)]
    #[test]
    fn keys_have_the_codes_windows_knows_them_by() {
        use egui::Key;
        let code = super::portal::virtual_key;
        assert_eq!(code(Key::A), Some(0x41));
        assert_eq!(code(Key::Num7), Some(0x37));
        assert_eq!(code(Key::F1), Some(0x70));
        assert_eq!(code(Key::F13), Some(0x7C));
        assert_eq!(code(Key::Space), Some(0x20));
        assert_eq!(code(Key::Comma), None, "its code depends on the layout");
    }

    #[test]
    fn losing_the_shortcut_releases_its_key() {
        let talk = Talk::default();
        talk.set_local(true, false);
        talk.set_global(GlobalKey::Ready {
            trigger: String::new(),
        });
        talk.set_global_key(true);
        talk.set_global(GlobalKey::Unavailable("it went away".into()));
        assert!(!talk.transmitting());
    }
}
