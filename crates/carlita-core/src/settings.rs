//! Application settings, applied immediately and saved on every change.

use crate::scaling::EditorScaling;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    /// Carla engine driver: "JACK" (also PipeWire via pipewire-jack), "ALSA", ...
    pub driver: String,
    /// Device name for drivers that have devices (ALSA, PulseAudio).
    pub device: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    /// Run every plugin in its own bridge process so a crashing plugin cannot
    /// take down the host or the audio of the rest of the chain.
    pub isolate_plugins: bool,
    /// Automatically connect the chain to the system capture/playback ports.
    pub auto_connect: bool,
    /// JACK client feeding the chain ("" = the physical "system" ports).
    pub input_client: String,
    /// JACK client the chain plays into ("" = the physical "system" ports).
    pub output_client: String,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            driver: "JACK".into(),
            device: String::new(),
            sample_rate: 48_000,
            buffer_size: 128,
            isolate_plugins: true,
            auto_connect: true,
            input_client: String::new(),
            output_client: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    pub fullscreen: bool,
    pub window_width: u32,
    pub window_height: u32,
    pub default_scaling: EditorScaling,
    /// Brightness 0..=100, if the hardware supports it.
    pub brightness: Option<u32>,
    /// Width of the invisible edge strips that open the sidebars, in pixels.
    pub edge_strip_width: u32,
    /// Forward touches to captured editors with XTest instead of synthetic events.
    pub xtest_input: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            fullscreen: true,
            window_width: 1280,
            window_height: 800,
            default_scaling: EditorScaling::Fit,
            brightness: None,
            edge_strip_width: 18,
            xtest_input: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiSettings {
    /// Name of the MIDI input port used for MIDI learn ("" = all ports).
    pub learn_input: String,
    /// Channel used by the on-screen keyboard (0-based).
    pub keyboard_channel: u8,
    /// Octave offset of the on-screen keyboard.
    pub keyboard_octave: i8,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub audio: AudioSettings,
    pub display: DisplaySettings,
    pub midi: MidiSettings,
    /// Preset loaded at startup.
    pub default_preset: Option<String>,
    /// Preset that was last loaded or saved (shown in the UI).
    pub current_preset: Option<String>,
    pub show_keyboard: bool,
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match crate::read_json(path) {
            Ok(Some(s)) => s,
            Ok(None) => Self::default(),
            Err(e) => {
                log::warn!(
                    "settings file {} is unreadable ({e}); using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::write_json(path, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"display":{"fullscreen":false}}"#).unwrap();
        assert!(!s.display.fullscreen);
        assert_eq!(s.display.window_width, 1280);
        assert_eq!(s.audio.driver, "JACK");
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings { default_preset: Some("Gig".into()), ..Default::default() };
        s.display.default_scaling = EditorScaling::Fixed(2.0);
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
    }
}
