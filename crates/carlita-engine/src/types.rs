//! Commands, events and data exchanged with the engine thread.

use carlita_core::{PluginDescriptor, SlotId};
use crossbeam_channel::Sender;

/// How to start the audio engine.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// Carla driver name: "JACK", "ALSA", "PulseAudio", "Dummy", ...
    pub driver: String,
    /// Device for drivers that have one (ALSA/PulseAudio); ignored for JACK.
    pub device: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    /// Run plugins in separate bridge processes (crash isolation).
    pub isolate_plugins: bool,
    /// JACK client whose outputs feed the chain input ("" = physical "system").
    pub input_client: String,
    /// JACK client the chain output is connected to ("" = physical "system").
    pub output_client: String,
    /// Connect inputs/outputs automatically after start.
    pub auto_connect: bool,
    pub client_name: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            driver: "JACK".into(),
            device: String::new(),
            sample_rate: 48_000,
            buffer_size: 128,
            isolate_plugins: true,
            input_client: String::new(),
            output_client: String::new(),
            auto_connect: true,
            client_name: "Carlita".into(),
        }
    }
}

/// A plugin to place into the chain.
#[derive(Debug, Clone)]
pub struct PluginLoad {
    pub slot: SlotId,
    pub plugin: PluginDescriptor,
    pub name: String,
    pub bypassed: bool,
    /// Carla state XML to restore after loading.
    pub state: Option<String>,
}

/// Requests sent to the engine thread.
#[derive(Debug)]
pub enum Command {
    Start(EngineConfig),
    Stop,
    QueryDrivers,
    /// Append a plugin, or insert it at `index`.
    AddPlugin {
        load: PluginLoad,
        index: Option<usize>,
    },
    RemovePlugin {
        slot: SlotId,
    },
    MovePlugin {
        from: usize,
        to: usize,
    },
    SetBypass {
        slot: SlotId,
        bypassed: bool,
    },
    /// Replace the whole chain (preset load). Plugins load one by one.
    LoadChain {
        loads: Vec<PluginLoad>,
    },
    ClearChain,
    /// Reload a crashed/failed plugin from its last known state.
    ReloadPlugin {
        load: PluginLoad,
    },
    /// Snapshot every plugin's state (for saving a preset).
    CaptureStates {
        reply: Sender<Vec<(SlotId, Option<String>)>>,
    },
    ShowEditor {
        slot: SlotId,
        show: bool,
    },
    /// Embed the editor into an X11 window (in-process plugins only).
    EmbedEditor {
        slot: SlotId,
        parent_xid: u64,
    },
    QueryParameters {
        slot: SlotId,
    },
    SetParameter {
        slot: SlotId,
        index: u32,
        value: f32,
    },
    /// Send a note to `slot`, or to the first plugin that accepts MIDI.
    SendNote {
        slot: Option<SlotId>,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Connect the chain to different JACK clients.
    Reconnect {
        input_client: String,
        output_client: String,
    },
    Shutdown,
}

/// Static facts about a loaded plugin.
#[derive(Debug, Clone, Default)]
pub struct PluginRuntimeInfo {
    pub has_custom_ui: bool,
    pub can_embed: bool,
    pub can_drywet: bool,
    pub is_bridge: bool,
    pub parameter_count: u32,
    pub audio_ins: u32,
    pub audio_outs: u32,
    pub midi_ins: u32,
}

/// A parameter for the generic editor page.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterInfo {
    pub index: u32,
    pub name: String,
    pub unit: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub value: f32,
    pub text: String,
    pub is_boolean: bool,
    pub is_integer: bool,
    pub is_output: bool,
}

/// An audio driver and its devices.
#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub name: String,
    pub devices: Vec<String>,
}

/// A JACK client with audio ports, usable as an input or output "device".
#[derive(Debug, Clone, PartialEq)]
pub struct ClientInfo {
    pub name: String,
    pub audio_outs: u32,
    pub audio_ins: u32,
}

/// Periodic meter readings (~30 Hz).
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Peak level entering the chain, left/right, 0..1.
    pub input: [f32; 2],
    /// Peak level leaving the chain, left/right, 0..1.
    pub output: [f32; 2],
    /// DSP load in percent.
    pub dsp_load: f32,
    pub xruns: u32,
    /// Per-slot plugin delay (PDC) in samples, in chain order.
    pub latencies: Vec<(SlotId, u32)>,
    pub sample_rate: f64,
    pub buffer_size: u32,
}

impl Meters {
    /// Total chain latency in milliseconds: one buffer in, one out, plus PDC.
    pub fn total_latency_ms(&self) -> f32 {
        if self.sample_rate <= 0.0 {
            return 0.0;
        }
        let pdc: u32 = self.latencies.iter().map(|(_, l)| *l).sum();
        ((2 * self.buffer_size + pdc) as f64 / self.sample_rate * 1000.0) as f32
    }
}

/// Notifications from the engine thread.
#[derive(Debug, Clone)]
pub enum Event {
    Started {
        driver: String,
        sample_rate: f64,
        buffer_size: u32,
    },
    StartFailed(String),
    Stopped,
    Drivers(Vec<DriverInfo>),
    Clients(Vec<ClientInfo>),
    PluginLoaded {
        slot: SlotId,
        info: PluginRuntimeInfo,
    },
    /// The plugin could not be loaded; a pass-through placeholder holds its place.
    PluginFailed {
        slot: SlotId,
        error: String,
    },
    /// The plugin was removed; `state` is its last state (for undo).
    PluginRemoved {
        slot: SlotId,
        state: Option<String>,
    },
    /// The plugin's bridge crashed or hung; it was replaced by a pass-through.
    PluginCrashed {
        slot: SlotId,
        message: String,
    },
    BypassChanged {
        slot: SlotId,
        bypassed: bool,
    },
    EditorClosed {
        slot: SlotId,
    },
    /// Embedded editor resized (width, height).
    EditorResized {
        slot: SlotId,
        width: i32,
        height: i32,
    },
    Parameters {
        slot: SlotId,
        params: Vec<ParameterInfo>,
    },
    ParameterChanged {
        slot: SlotId,
        index: u32,
        value: f32,
    },
    /// Chain finished loading (after `LoadChain`).
    ChainLoaded,
    Meters(Meters),
    Info(String),
    Error(String),
}

/// Unique window title given to each plugin editor, so the X11 editor host
/// can find the window.
pub fn editor_window_title(slot: SlotId) -> String {
    format!("carlita-editor-{}", slot.0)
}
