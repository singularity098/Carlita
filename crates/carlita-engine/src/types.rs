//! Commands, events and data exchanged with the engine thread.

use carlita_core::rig::RigLayout;
use carlita_core::{PluginDescriptor, SlotId, TrackId};
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
    /// JACK client whose outputs are the hardware inputs tracks pick from
    /// ("" = physical "system").
    pub input_client: String,
    /// JACK client the rig output is connected to ("" = physical "system").
    pub output_client: String,
    /// Under JACK, connect the hardware inputs, outputs and MIDI devices.
    pub auto_connect: bool,
    pub client_name: String,
    /// The scale Carlita draws at (the desktop's), passed to plugin editors
    /// so they draw at the same size.
    pub ui_scale: f32,
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
            ui_scale: 1.0,
        }
    }
}

/// A plugin to load. Where it sits in the rig comes from the layout.
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
    /// Load a plugin. It is wired up once the layout mentions it.
    AddPlugin {
        load: PluginLoad,
    },
    RemovePlugin {
        slot: SlotId,
    },
    /// Set the tracks and how their plugins are connected. Takes effect
    /// while audio keeps running.
    SetLayout(RigLayout),
    SetBypass {
        slot: SlotId,
        bypassed: bool,
    },
    /// Replace every plugin (preset load). Plugins load one by one.
    LoadRig {
        loads: Vec<PluginLoad>,
        layout: RigLayout,
    },
    /// Remove every plugin; the tracks stay.
    ClearRig,
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
    /// Send a note to `slot`, or else to the first plugin that accepts MIDI
    /// in `track` (or in the whole rig).
    SendNote {
        slot: Option<SlotId>,
        track: Option<TrackId>,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// The internal graph's connections as (from, to) group names, one per
    /// connected pair of groups (for diagnostics and tests).
    DescribeGraph {
        reply: Sender<Vec<(String, String)>>,
    },
    /// Meter the first `count` hardware inputs one by one (0 = stop), for
    /// showing which inputs have a signal while picking a track's inputs.
    ProbeInputs {
        count: u16,
    },
    /// Connect the rig to different JACK clients.
    Reconnect {
        input_client: String,
        output_client: String,
    },
    /// How often to send [`Event::Meters`]: the display's frame time, so
    /// meters move at the display's rate (clamped to 4..=50 ms).
    SetMeterInterval(std::time::Duration),
    /// The JACK client that records stems (JACK only), or None. Its input
    /// ports named by [`recorder_port`] are wired to each track's output
    /// and to the hardware inputs the track takes.
    SetRecorder(Option<String>),
    Shutdown,
}

/// Name of a stem recorder input port: channel `channel` (0-based) of
/// `track`'s output, or of its dry input when `dry`.
pub fn recorder_port(track: TrackId, dry: bool, channel: usize) -> String {
    format!(
        "t{}_{}_{}",
        track.0,
        if dry { "dry" } else { "out" },
        channel + 1
    )
}

/// Static facts about a loaded plugin.
#[derive(Debug, Clone, Default)]
pub struct PluginRuntimeInfo {
    pub has_custom_ui: bool,
    pub can_embed: bool,
    /// The plugin says its editor can be resized (VST3 `canResize`, CLAP
    /// `can_resize`, LV2 without `ui:noUserResize`).
    pub resizable_ui: bool,
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

/// A hardware MIDI device (one of its ports) that tracks can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiDevice {
    /// Full port name, "client:port": what a track stores.
    pub id: String,
    /// The name to show.
    pub name: String,
}

/// Periodic meter readings (~30 Hz).
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Peak level entering the rig (the loudest track input), left/right, 0..1.
    pub input: [f32; 2],
    /// Peak level leaving the rig, left/right, 0..1.
    pub output: [f32; 2],
    /// Peak level of each track's bus (after its volume), left/right.
    pub tracks: Vec<(TrackId, [f32; 2])>,
    /// Peak level of the inputs each track takes, left/right.
    pub track_inputs: Vec<(TrackId, [f32; 2])>,
    /// Peak level of each hardware input, 0..1, while they are probed
    /// (`Command::ProbeInputs`); empty otherwise.
    pub hardware_inputs: Vec<f32>,
    /// DSP load in percent.
    pub dsp_load: f32,
    pub xruns: u32,
    /// Per-slot plugin delay (PDC) in samples.
    pub latencies: Vec<(SlotId, u32)>,
    /// The longest plugin delay through the rig, in samples.
    pub path_latency: u32,
    pub sample_rate: f64,
    pub buffer_size: u32,
}

impl Meters {
    /// Total latency in milliseconds: one buffer in, one out, plus the
    /// plugin delay along the longest path.
    pub fn total_latency_ms(&self) -> f32 {
        if self.sample_rate <= 0.0 {
            return 0.0;
        }
        ((2 * self.buffer_size + self.path_latency) as f64 / self.sample_rate * 1000.0) as f32
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
    /// The hardware inputs and MIDI devices tracks can pick from, sent when
    /// they change.
    Hardware {
        /// Port name of each input channel, in order.
        inputs: Vec<String>,
        midi: Vec<MidiDevice>,
        /// MIDI devices cannot be told apart (not under JACK): every track
        /// takes all of them.
        merged_midi: bool,
    },
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
    /// Every plugin finished loading (after `LoadRig`).
    RigLoaded,
    Meters(Meters),
    Info(String),
    Error(String),
}

/// Unique window title given to each plugin editor, so the X11 editor host
/// can find the window.
pub fn editor_window_title(slot: SlotId) -> String {
    format!("carlita-editor-{}", slot.0)
}
