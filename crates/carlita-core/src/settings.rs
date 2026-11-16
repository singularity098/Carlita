//! Application settings, applied immediately and saved on every change.

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
    /// How many hardware inputs the interface really has, so tracks are
    /// not offered ports that are not there (0 = every port reported).
    pub input_count: u32,
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
            input_count: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplaySettings {
    pub fullscreen: bool,
    /// Size of the window when it is neither fullscreen nor maximized
    /// (logical pixels).
    pub window_width: u32,
    pub window_height: u32,
    /// Where that window was on the desktop (physical pixels), if known.
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
    /// Whether the window was maximized (kept while fullscreen, for when
    /// fullscreen is left).
    pub maximized: bool,
    /// Brightness 0..=100, if the hardware supports it.
    pub brightness: Option<u32>,
    /// Width of the invisible edge strips that open the sidebars, in pixels.
    /// Kept narrow, so buttons near the screen's edges stay reachable by
    /// touch. (Renamed from `edge_strip_width`, whose old default of 18 was
    /// too wide, so saved settings pick up the new default.)
    pub edge_swipe_width: u32,
    /// The UI theme, by name (`Theme.names` in `ui/theme.slint`; unknown
    /// names fall back to the first, Iris). Never rename a theme.
    pub theme: String,
    /// How large plugin editors are drawn, in percent of the desktop's
    /// scale (passed to plugins as their UI scale). Touchscreens want more.
    pub editor_zoom: u32,
}

impl DisplaySettings {
    /// The editor zooms offered in Display options, in percent.
    pub const EDITOR_ZOOMS: [u32; 5] = [100, 125, 150, 175, 200];

    /// `editor_zoom` as a factor, kept to what is offered (100%..200%).
    pub fn editor_zoom_factor(&self) -> f32 {
        self.editor_zoom.clamp(100, 200) as f32 / 100.0
    }
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            fullscreen: true,
            window_width: 1280,
            window_height: 800,
            window_x: None,
            window_y: None,
            maximized: false,
            brightness: None,
            edge_swipe_width: 6,
            theme: "Iris".into(),
            editor_zoom: 100,
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
pub struct RecordingSettings {
    /// Folder new recording projects are made in ("" = the default,
    /// `AppPaths::default_projects_dir`).
    pub projects_dir: String,
    /// Also record each track's inputs before its plugins.
    pub record_dry: bool,
    /// File format of the stems.
    pub format: RecordFormat,
}

/// File format of recorded stems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordFormat {
    /// 32-bit float WAV (RF64 past 4 GB): exactly what the tracks played,
    /// peaks over 0 dBFS included.
    #[default]
    Wav,
    /// 24-bit FLAC: about half the size, lossless below 0 dBFS (louder
    /// peaks are clipped).
    Flac,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub audio: AudioSettings,
    pub display: DisplaySettings,
    pub midi: MidiSettings,
    pub recording: RecordingSettings,
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
        assert_eq!(s.display.editor_zoom, 100);
        assert_eq!(s.display.editor_zoom_factor(), 1.0);
    }

    #[test]
    fn settings_from_before_editor_embedding_still_load() {
        let s: Settings = serde_json::from_str(
            r#"{"display":{"fullscreen":false,"default_scaling":"Fit","xtest_input":true}}"#,
        )
        .unwrap();
        assert!(!s.display.fullscreen);
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings {
            default_preset: Some("Gig".into()),
            ..Default::default()
        };
        s.display.theme = "Abyss".into();
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
    }
}
