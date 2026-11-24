//! What the user has chosen on the audio panel, as it is saved between runs.

use flowerchat_client::audio::{AudioDevice, AudioDevices, AudioSettings};
use serde::{Deserialize, Serialize};

/// The top of the receive buffer slider.
pub const MAX_RECEIVE_BUFFER_MS: u32 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The microphone to use, or whatever the system's default is.
    pub microphone: Option<DeviceChoice>,
    pub speakers: Option<DeviceChoice>,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub automatic_volume: bool,
    pub receive_buffer_ms: u32,
    pub send: SendMode,
    /// The key that sends voice while this window has focus, by the name
    /// egui gives it.
    pub talk_key: Option<String>,
}

/// A device the user picked. The label is kept so that the device can still
/// be named when it is not plugged in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceChoice {
    pub id: String,
    pub label: String,
}

impl From<&AudioDevice> for DeviceChoice {
    fn from(device: &AudioDevice) -> Self {
        Self {
            id: device.id.clone(),
            label: device.label.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
    /// The microphone is sent all the time.
    #[default]
    Always,
    /// The microphone is sent while a key is held: push-to-talk.
    WhileKeyHeld,
}

impl Default for Settings {
    fn default() -> Self {
        let audio = AudioSettings::default();
        Self {
            microphone: None,
            speakers: None,
            echo_cancellation: audio.echo_cancellation,
            noise_suppression: audio.noise_suppression,
            automatic_volume: audio.automatic_volume,
            receive_buffer_ms: audio.jitter_ms,
            send: SendMode::Always,
            talk_key: None,
        }
    }
}

impl Settings {
    /// What the engine should use, given what is plugged in. A chosen device
    /// that is missing gives way to the system default until it is back.
    pub fn audio(&self, devices: &AudioDevices) -> AudioSettings {
        AudioSettings {
            input: present(&self.microphone, &devices.inputs),
            output: present(&self.speakers, &devices.outputs),
            echo_cancellation: self.echo_cancellation,
            noise_suppression: self.noise_suppression,
            automatic_volume: self.automatic_volume,
            jitter_ms: self.receive_buffer_ms,
        }
    }

    pub fn talk_key(&self) -> Option<egui::Key> {
        self.talk_key.as_deref().and_then(egui::Key::from_name)
    }
}

/// The id of `choice` if it is among `devices`.
fn present(choice: &Option<DeviceChoice>, devices: &[AudioDevice]) -> Option<String> {
    let choice = choice.as_ref()?;
    // An empty list means the devices could not be listed at all, which is
    // no reason to distrust the choice.
    let known = devices.is_empty() || devices.iter().any(|device| device.id == choice.id);
    known.then(|| choice.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            label: id.to_uppercase(),
            is_default: false,
        }
    }

    #[test]
    fn a_missing_device_gives_way_to_the_default() {
        let headset = device("headset");
        let settings = Settings {
            microphone: Some(DeviceChoice::from(&headset)),
            ..Settings::default()
        };
        let mut devices = AudioDevices {
            inputs: vec![device("webcam")],
            outputs: vec![],
        };
        assert_eq!(settings.audio(&devices).input, None);
        devices.inputs.push(headset);
        assert_eq!(settings.audio(&devices).input.as_deref(), Some("headset"));
    }

    #[test]
    fn a_choice_is_trusted_when_devices_cannot_be_listed() {
        let settings = Settings {
            speakers: Some(DeviceChoice::from(&device("hdmi"))),
            ..Settings::default()
        };
        let audio = settings.audio(&AudioDevices::default());
        assert_eq!(audio.output.as_deref(), Some("hdmi"));
    }

    #[test]
    fn the_talk_key_survives_being_saved_by_name() {
        let settings = Settings {
            talk_key: Some(egui::Key::F8.name().into()),
            ..Settings::default()
        };
        assert_eq!(settings.talk_key(), Some(egui::Key::F8));
    }
}
