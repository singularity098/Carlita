//! Plugin descriptions as found by the scanner.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Plugin formats Carlita can host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    Lv2,
    Vst2,
    Vst3,
    Clap,
    Ladspa,
    /// Carla's built-in plugins (gain, MIDI tools, ...).
    Internal,
}

impl PluginFormat {
    pub const ALL: [PluginFormat; 6] = [
        PluginFormat::Lv2,
        PluginFormat::Vst2,
        PluginFormat::Vst3,
        PluginFormat::Clap,
        PluginFormat::Ladspa,
        PluginFormat::Internal,
    ];

    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            PluginFormat::Lv2 => "LV2",
            PluginFormat::Vst2 => "VST2",
            PluginFormat::Vst3 => "VST3",
            PluginFormat::Clap => "CLAP",
            PluginFormat::Ladspa => "LADSPA",
            PluginFormat::Internal => "Internal",
        }
    }

    /// The type argument `carla-discovery-*` expects.
    pub fn discovery_arg(self) -> &'static str {
        match self {
            PluginFormat::Lv2 => "lv2",
            PluginFormat::Vst2 => "vst2",
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Clap => "clap",
            PluginFormat::Ladspa => "ladspa",
            PluginFormat::Internal => "internal",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.label().eq_ignore_ascii_case(s))
    }
}

impl fmt::Display for PluginFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Binary architecture of a plugin. Windows plugins loaded through yabridge
/// appear as native Linux plugins, so `Native` covers them as well; the
/// Windows variants are only for Carla's own Wine bridges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BinaryArch {
    #[default]
    Native,
    Win32,
    Win64,
}

/// Plugin category, mirroring Carla's `PluginCategory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginCategory {
    Synth,
    Delay,
    Eq,
    Filter,
    Distortion,
    Dynamics,
    Modulator,
    Utility,
    #[default]
    Other,
}

impl PluginCategory {
    pub const ALL: [PluginCategory; 9] = [
        PluginCategory::Synth,
        PluginCategory::Delay,
        PluginCategory::Eq,
        PluginCategory::Filter,
        PluginCategory::Distortion,
        PluginCategory::Dynamics,
        PluginCategory::Modulator,
        PluginCategory::Utility,
        PluginCategory::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PluginCategory::Synth => "Synth",
            PluginCategory::Delay => "Delay",
            PluginCategory::Eq => "EQ",
            PluginCategory::Filter => "Filter",
            PluginCategory::Distortion => "Distortion",
            PluginCategory::Dynamics => "Dynamics",
            PluginCategory::Modulator => "Modulator",
            PluginCategory::Utility => "Utility",
            PluginCategory::Other => "Other",
        }
    }

    /// Parses the category names printed by `carla-discovery`.
    pub fn from_discovery(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "synth" => PluginCategory::Synth,
            "delay" => PluginCategory::Delay,
            "eq" => PluginCategory::Eq,
            "filter" => PluginCategory::Filter,
            "distortion" => PluginCategory::Distortion,
            "dynamics" => PluginCategory::Dynamics,
            "modulator" => PluginCategory::Modulator,
            "utility" => PluginCategory::Utility,
            _ => PluginCategory::Other,
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|c| c.label().eq_ignore_ascii_case(s))
    }
}

/// Everything needed to list a plugin and load it into Carla.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    pub format: PluginFormat,
    #[serde(default)]
    pub arch: BinaryArch,
    pub name: String,
    /// Plugin label / URI (LV2 URI, internal label, VST3 class id, ...).
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub maker: String,
    /// Binary or bundle path; empty for LV2 and internal plugins.
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub unique_id: i64,
    #[serde(default)]
    pub category: PluginCategory,
    #[serde(default)]
    pub audio_ins: u32,
    #[serde(default)]
    pub audio_outs: u32,
    #[serde(default)]
    pub midi_ins: u32,
    #[serde(default)]
    pub midi_outs: u32,
    #[serde(default)]
    pub parameters: u32,
    #[serde(default)]
    pub is_synth: bool,
    #[serde(default)]
    pub has_custom_ui: bool,
    /// True for plugins that live in a yabridge directory (bridged Windows plugins).
    #[serde(default)]
    pub yabridge: bool,
}

impl PluginDescriptor {
    /// Stable key used for favorites and de-duplication.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.format.discovery_arg(),
            self.filename,
            self.label,
            self.unique_id
        )
    }

    /// Lowercase text the search box matches against.
    pub fn search_text(&self) -> String {
        format!(
            "{} {} {} {}",
            self.name,
            self.maker,
            self.label,
            self.format.label()
        )
        .to_lowercase()
    }
}
