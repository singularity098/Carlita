//! The engine thread: owns the Carla host and runs its idle loop.

use crate::carla::{self, Host, RawCallback};
use crate::types::*;
use carla_sys as sys;
use carlita_core::discovery::default_search_paths;
use carlita_core::rig::{self, LinkKind, Node, NodeIo, PortSel, RigLayout};
use carlita_core::{PluginFormat, SlotId, TrackId};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(16);
const METER_INTERVAL: Duration = Duration::from_millis(33);
const CONNECT_DELAY: Duration = Duration::from_millis(400);
/// Carla's internal stereo gain at unity: a pass-through placeholder for
/// plugins that failed to load or crashed, so their path keeps sounding, and
/// the bus that meters the rig input, the rig output and each track.
const PLACEHOLDER_LABEL: &str = "audiogain_s";
/// Parameter 0 of `audiogain_s`: linear gain, 0..=4.
const GAIN_PARAM: u32 = 0;
/// `PatchbayIcon::PATCHBAY_ICON_PLUGIN`: the group of a plugin.
const ICON_PLUGIN: i32 = 1;
/// Markers put into the callback queue before a patchbay refresh, so the
/// callbacks that follow are filed under the right graph.
const MARK_EXTERNAL: u32 = u32::MAX - 1;
const MARK_INTERNAL: u32 = u32::MAX - 2;
/// Groups of Carla's internal graph for the engine's own ports.
const GROUP_AUDIO_IN: &str = "Audio Input";
const GROUP_AUDIO_OUT: &str = "Audio Output";
const GROUP_MIDI_IN: &str = "Midi Input";
const GROUP_MIDI_OUT: &str = "Midi Output";

/// Handle to the engine thread. Dropping it shuts the engine down.
pub struct EngineHandle {
    tx: Sender<Command>,
    events: Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EngineHandle {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let thread = std::thread::Builder::new()
            .name("carlita-engine".into())
            .spawn(move || Engine::new(ev_tx).run(rx))
            .expect("spawn engine thread");
        Self {
            tx,
            events: ev_rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Receiver for engine events. Poll it from the UI thread.
    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }

    /// Path of Carla's plugin discovery tool.
    pub fn discovery_tool() -> std::path::PathBuf {
        carla::discovery_tool()
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What a Carla plugin is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    OutputBus,
    /// The bus at the end of a track (volume, mute, level).
    TrackBus(TrackId),
    /// Meters the inputs a track takes; its outputs go nowhere.
    TrackInputBus(TrackId),
    /// Meters two hardware inputs (2k and 2k+1) on its own; its outputs go
    /// nowhere.
    InputProbe(u16),
    Plugin(SlotId),
}

/// A Carla plugin. `Engine::slots[i]` is Carla plugin id `i`.
struct Slot {
    owner: Owner,
    /// The name Carla knows it by; under JACK, also its JACK client's name.
    name: String,
    /// What was loaded (user plugins only).
    load: Option<PluginLoad>,
    placeholder: bool,
    info: PluginRuntimeInfo,
    /// Group of the plugin in Carla's internal graph.
    group: Option<u32>,
}

impl Slot {
    fn id(&self) -> Option<SlotId> {
        match self.owner {
            Owner::Plugin(id) => Some(id),
            _ => None,
        }
    }

    fn bypassed(&self) -> bool {
        self.load.as_ref().is_some_and(|l| l.bypassed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Internal,
    External,
}

#[derive(Default)]
struct Patchbay {
    clients: HashMap<u32, String>,
    /// (client, port) -> (name, hints)
    ports: HashMap<(u32, u32), (String, u32)>,
    /// connection id -> (out client, out port, in client, in port)
    connections: HashMap<u32, (u32, u32, u32, u32)>,
}

impl Patchbay {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn client_id(&self, name: &str) -> Option<u32> {
        self.clients
            .iter()
            .find(|(_, n)| n.as_str() == name)
            .map(|(id, _)| *id)
    }

    /// Ports of `client` matching `filter(hints)`, ordered by port id.
    fn ports_of(&self, client: u32, filter: impl Fn(u32) -> bool) -> Vec<(u32, String)> {
        let mut v: Vec<(u32, String)> = self
            .ports
            .iter()
            .filter(|((c, _), (_, hints))| *c == client && filter(*hints))
            .map(|((_, p), (name, _))| (*p, name.clone()))
            .collect();
        v.sort_by_key(|(p, _)| *p);
        v
    }

    fn clients_info(&self, own: &str) -> Vec<ClientInfo> {
        let mut v: Vec<ClientInfo> = self
            .clients
            .iter()
            .filter(|(_, n)| !is_own_client(n, own))
            .map(|(id, name)| ClientInfo {
                name: name.clone(),
                audio_outs: self.ports_of(*id, |h| is_audio(h) && !is_input(h)).len() as u32,
                audio_ins: self.ports_of(*id, |h| is_audio(h) && is_input(h)).len() as u32,
            })
            .filter(|c| c.audio_ins + c.audio_outs > 0)
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn is_jack_driver(driver: &str) -> bool {
    driver.to_ascii_lowercase().starts_with("jack")
}

/// Whether a JACK client is Carlita's own: its main client or one of its
/// plugins' clients ("Carlita.…").
fn is_own_client(name: &str, own: &str) -> bool {
    name == own || name.strip_prefix(own).is_some_and(|r| r.starts_with('.'))
}

/// Whether a JACK client carries hardware MIDI (JACK "system", PipeWire
/// "Midi-Bridge", a2jmidid "a2j").
fn is_midi_hardware(client: &str) -> bool {
    client == "system" || client.starts_with("Midi-Bridge") || client.starts_with("a2j")
}

/// Whether JACK client `name` is the one opened for plugin name `expected`.
/// JACK appends "-01" and so on when the name is still taken (a crashed
/// plugin's client may still be closing).
fn client_matches(name: &str, expected: &str) -> bool {
    name.strip_prefix(expected).is_some_and(|r| {
        r.is_empty()
            || (r.len() > 1 && r.starts_with('-') && r[1..].chars().all(|c| c.is_ascii_digit()))
    })
}

/// A MIDI port's name as shown to the user: the device's own name for the
/// port, without the audio server's decoration. "Arturia KeyStep 32:
/// (capture_0) KeyStep 32 MIDI 1" (PipeWire) becomes "KeyStep 32 MIDI 1",
/// "KeyStep [24] (capture): KeyStep MIDI 1" (a2jmidid) "KeyStep MIDI 1".
fn midi_label(port: &str) -> String {
    let cut = [") ", ": "]
        .iter()
        .filter_map(|sep| port.rfind(sep).map(|i| i + sep.len()))
        .max();
    match cut {
        Some(i) if i < port.len() => port[i..].trim().to_string(),
        _ => port.to_string(),
    }
}

/// Keeps a name to characters JACK and Carla take as they are.
fn clean_name(name: &str, max: usize) -> String {
    name.chars()
        .map(|c| if c == ':' || c == '/' { '.' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn is_input(hints: u32) -> bool {
    hints & sys::PATCHBAY_PORT_IS_INPUT != 0
}
fn is_audio(hints: u32) -> bool {
    hints & sys::PATCHBAY_PORT_TYPE_AUDIO != 0
}
fn is_midi(hints: u32) -> bool {
    hints & sys::PATCHBAY_PORT_TYPE_MIDI != 0
}

struct Engine {
    host: Host,
    events: Sender<Event>,
    raw_rx: Receiver<RawCallback>,
    // Boxed so its address stays stable for Carla's callback pointer.
    raw_tx: Box<Sender<RawCallback>>,
    running: bool,
    /// Carla runs one JACK client per plugin (under JACK), and the rig is
    /// wired in the JACK graph itself. Otherwise it is wired in Carla's
    /// internal graph.
    multi: bool,
    /// The hardware inputs and MIDI devices last reported to the UI.
    hardware: Option<(Vec<String>, Vec<MidiDevice>)>,
    /// How many hardware inputs to meter one by one (0 = none).
    probe_inputs: u16,
    config: EngineConfig,
    slots: Vec<Slot>,
    layout: RigLayout,
    pending_loads: VecDeque<PluginLoad>,
    loading_rig: bool,
    /// Carla's internal graph: our plugins and how they are connected.
    internal: Patchbay,
    /// The JACK graph (other clients, for devices and auto-connect).
    external: Patchbay,
    /// Which graph the patchbay callbacks currently describe.
    view: View,
    /// The connections need to be brought in line with the layout.
    dirty: bool,
    connect_at: Option<Instant>,
    last_meters: Instant,
    meter_interval: Duration,
    /// The stem recorder's JACK client name (see `Command::SetRecorder`).
    recorder: Option<String>,
    state_dir: tempfile::TempDir,
}

impl Engine {
    fn new(events: Sender<Event>) -> Self {
        let (raw_tx, raw_rx) = unbounded();
        let raw_tx = Box::new(raw_tx);
        let host = Host::new();
        host.set_callback(&*raw_tx as *const _);
        Self {
            host,
            events,
            raw_rx,
            raw_tx,
            running: false,
            multi: false,
            hardware: None,
            probe_inputs: 0,
            config: EngineConfig::default(),
            slots: Vec::new(),
            layout: RigLayout::default(),
            pending_loads: VecDeque::new(),
            loading_rig: false,
            internal: Patchbay::default(),
            external: Patchbay::default(),
            view: View::Internal,
            dirty: false,
            connect_at: None,
            last_meters: Instant::now(),
            meter_interval: METER_INTERVAL,
            recorder: None,
            state_dir: tempfile::Builder::new()
                .prefix("carlita-state")
                .tempdir()
                .expect("temp dir"),
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn run(mut self, rx: Receiver<Command>) {
        loop {
            match rx.recv_timeout(TICK.min(self.meter_interval)) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    self.handle(cmd);
                    // Handle anything else queued before idling.
                    while let Ok(cmd) = rx.try_recv() {
                        if matches!(cmd, Command::Shutdown) {
                            self.stop();
                            return;
                        }
                        self.handle(cmd);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        self.stop();
    }

    fn tick(&mut self) {
        if !self.running {
            return;
        }
        self.host.idle();
        self.drain();
        if self.connect_at.is_some_and(|t| Instant::now() >= t) {
            self.connect_at = None;
            self.emit_clients();
            self.dirty = true;
        }
        // Load queued plugins one per tick so the UI keeps updating.
        if let Some(load) = self.pending_loads.pop_front() {
            self.add_plugin(load);
            if self.pending_loads.is_empty() && self.loading_rig {
                self.loading_rig = false;
                self.emit(Event::RigLoaded);
            }
        }
        if self.dirty {
            self.rewire();
        }
        if self.last_meters.elapsed() >= self.meter_interval {
            self.last_meters = Instant::now();
            self.emit(Event::Meters(self.meters()));
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Start(config) => self.start(config),
            Command::Stop => self.stop(),
            Command::QueryDrivers => {
                self.emit(Event::Drivers(carla::drivers()));
                if self.running && self.multi {
                    self.emit_clients();
                }
            }
            Command::AddPlugin { load } => {
                if self.require_running() {
                    self.add_plugin(load)
                }
            }
            Command::RemovePlugin { slot } => self.remove_plugin(slot),
            Command::SetLayout(layout) => self.set_layout(layout),
            Command::SetBypass { slot, bypassed } => self.set_bypass(slot, bypassed),
            Command::LoadRig { loads, layout } => {
                self.clear_rig();
                self.set_layout(layout);
                if self.require_running() {
                    self.loading_rig = true;
                    self.pending_loads = loads.into();
                    if self.pending_loads.is_empty() {
                        self.loading_rig = false;
                        self.emit(Event::RigLoaded);
                    }
                }
            }
            Command::ClearRig => self.clear_rig(),
            Command::ReloadPlugin { load } => self.reload_plugin(load),
            Command::CaptureStates { reply } => {
                let states = self.capture_states();
                let _ = reply.send(states);
            }
            Command::ShowEditor { slot, show } => {
                if let Some(i) = self.index_of(slot) {
                    if self.slots[i].info.has_custom_ui && !self.slots[i].placeholder {
                        self.host.set_ui_title(i as u32, &editor_window_title(slot));
                        self.host.show_ui(i as u32, show);
                    }
                }
            }
            Command::EmbedEditor { slot, parent_xid } => {
                if let Some(i) = self.index_of(slot) {
                    if self.slots[i].info.can_embed {
                        self.host.embed_ui(i as u32, parent_xid);
                    }
                }
            }
            Command::QueryParameters { slot } => {
                if let Some(i) = self.index_of(slot) {
                    let n = self.host.parameter_count(i as u32).min(512);
                    let params = (0..n).map(|p| self.host.parameter(i as u32, p)).collect();
                    self.emit(Event::Parameters { slot, params });
                }
            }
            Command::SetParameter { slot, index, value } => {
                if let Some(i) = self.index_of(slot) {
                    self.host.set_parameter(i as u32, index, value);
                }
            }
            Command::SendNote {
                slot,
                track,
                channel,
                note,
                velocity,
            } => {
                let target = match slot {
                    Some(s) => self.index_of(s),
                    None => self.first_midi_plugin(track),
                };
                if let Some(i) = target {
                    self.host
                        .send_note(i as u32, channel & 0x0F, note & 0x7F, velocity & 0x7F);
                }
            }
            Command::ProbeInputs { count } => {
                self.probe_inputs = count.min(rig::MAX_INPUTS as u16);
                self.sync_buses();
            }
            Command::Reconnect {
                input_client,
                output_client,
            } => {
                self.config.input_client = input_client;
                self.config.output_client = output_client;
                self.dirty = true;
            }
            Command::SetMeterInterval(interval) => {
                self.meter_interval =
                    interval.clamp(Duration::from_millis(4), Duration::from_millis(50));
            }
            Command::SetRecorder(name) => {
                self.recorder = name;
                self.dirty = true;
            }
            Command::DescribeGraph { reply } => {
                let graph = self.routing();
                let name = |g: u32| {
                    graph
                        .clients
                        .get(&g)
                        .cloned()
                        .unwrap_or_else(|| g.to_string())
                };
                let mut edges: Vec<(String, String)> = graph
                    .connections
                    .values()
                    .map(|(og, _, ig, _)| (name(*og), name(*ig)))
                    .collect();
                edges.sort();
                edges.dedup();
                let _ = reply.send(edges);
            }
            Command::Shutdown => unreachable!("handled in run loop"),
        }
    }

    fn require_running(&self) -> bool {
        if !self.running {
            self.emit(Event::Error("The audio engine is not running".into()));
        }
        self.running
    }

    fn index_of(&self, slot: SlotId) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.owner == Owner::Plugin(slot))
    }

    fn slot_of(&self, owner: Owner) -> Option<&Slot> {
        self.slots.iter().find(|s| s.owner == owner)
    }

    /// The first running plugin that takes MIDI, in signal order.
    fn first_midi_plugin(&self, track: Option<TrackId>) -> Option<usize> {
        let tracks = self
            .layout
            .tracks
            .iter()
            .filter(|t| !matches!(track, Some(id) if id != t.id));
        let order: Vec<SlotId> = RigLayout {
            tracks: tracks.cloned().collect(),
        }
        .slot_ids()
        .collect();
        let found = order.iter().find_map(|id| {
            let i = self.index_of(*id)?;
            let s = &self.slots[i];
            (!s.placeholder && s.info.midi_ins > 0).then_some(i)
        });
        match found {
            None if track.is_some() => self.first_midi_plugin(None),
            f => f,
        }
    }

    // ---------------------------------------------------------------- engine

    fn start(&mut self, config: EngineConfig) {
        if self.running {
            self.stop();
        }
        let multi = is_jack_driver(&config.driver);
        let h = &self.host;
        // Under JACK every plugin gets its own JACK client, so each track
        // can take any hardware inputs and MIDI devices: Carla's own client
        // has only two inputs and one MIDI input that merges every device.
        h.set_option(
            sys::ENGINE_OPTION_PROCESS_MODE,
            if multi {
                sys::ENGINE_PROCESS_MODE_MULTIPLE_CLIENTS
            } else {
                sys::ENGINE_PROCESS_MODE_PATCHBAY
            } as i32,
            None,
        );
        h.set_option(
            sys::ENGINE_OPTION_TRANSPORT_MODE,
            sys::ENGINE_TRANSPORT_MODE_INTERNAL as i32,
            None,
        );
        h.set_option(sys::ENGINE_OPTION_FORCE_STEREO, 1, None);
        h.set_option(
            sys::ENGINE_OPTION_PREFER_PLUGIN_BRIDGES,
            config.isolate_plugins as i32,
            None,
        );
        h.set_option(sys::ENGINE_OPTION_PREFER_UI_BRIDGES, 1, None);
        h.set_option(sys::ENGINE_OPTION_UIS_ALWAYS_ON_TOP, 0, None);
        // Carla takes the scale in thousandths. Without it, editors draw at
        // 1.0 however the desktop is scaled.
        let ui_scale = config.ui_scale.clamp(0.5, 4.0);
        h.set_option(
            sys::ENGINE_OPTION_FRONTEND_UI_SCALE,
            (ui_scale * 1000.0).round() as i32,
            None,
        );
        // Carla does not hand that option to plugin bridges. The AppImage's
        // bridge is patched to read it from here instead (bridges inherit
        // Carlita's environment); see packaging/appimage/carla-patches.
        std::env::set_var("CARLA_BRIDGE_UI_SCALE", format!("{ui_scale:.3}"));
        h.set_option(
            sys::ENGINE_OPTION_AUDIO_BUFFER_SIZE,
            config.buffer_size as i32,
            None,
        );
        h.set_option(
            sys::ENGINE_OPTION_AUDIO_SAMPLE_RATE,
            config.sample_rate as i32,
            None,
        );
        if !config.device.is_empty() {
            h.set_option(sys::ENGINE_OPTION_AUDIO_DEVICE, 0, Some(&config.device));
        }
        h.set_option(
            sys::ENGINE_OPTION_PATH_BINARIES,
            0,
            Some(&carla::library_folder().to_string_lossy()),
        );
        h.set_option(
            sys::ENGINE_OPTION_PATH_RESOURCES,
            0,
            Some(&carla::resources_folder().to_string_lossy()),
        );
        for (format, ptype) in [
            (PluginFormat::Ladspa, sys::PLUGIN_LADSPA),
            (PluginFormat::Vst2, sys::PLUGIN_VST2),
            (PluginFormat::Vst3, sys::PLUGIN_VST3),
        ] {
            let paths = std::env::join_paths(default_search_paths(format)).unwrap_or_default();
            h.set_option(
                sys::ENGINE_OPTION_PLUGIN_PATH,
                ptype as i32,
                Some(&paths.to_string_lossy()),
            );
        }
        let lv2 = std::env::var("LV2_PATH").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/.lv2:/usr/local/lib/lv2:/usr/lib/lv2")
        });
        h.set_option(
            sys::ENGINE_OPTION_PLUGIN_PATH,
            sys::PLUGIN_LV2 as i32,
            Some(&lv2),
        );

        if config.driver.eq_ignore_ascii_case("jack")
            && std::env::var_os("PIPEWIRE_QUANTUM").is_none()
        {
            // When libjack is PipeWire's, this requests the buffer size and
            // sample rate for our client. Real JACK ignores it.
            std::env::set_var(
                "PIPEWIRE_QUANTUM",
                format!("{}/{}", config.buffer_size, config.sample_rate),
            );
        }

        self.internal.clear();
        self.external.clear();
        self.view = View::Internal;
        self.hardware = None;
        self.config = config.clone();
        match self.host.init(&config.driver, &config.client_name) {
            Ok(()) => {
                self.running = true;
                self.multi = multi;
                self.drain();
                if multi {
                    // The JACK graph is the rig's graph from now on.
                    self.mark(MARK_EXTERNAL);
                    self.host.patchbay_refresh(true);
                }
                self.drain();
                self.add_bus(Owner::OutputBus);
                self.sync_buses();
                self.dirty = true;
                let (sr, bs) = (self.host.sample_rate(), self.host.buffer_size());
                log::info!(
                    "engine started: {} @ {sr} Hz, {bs} frames{}",
                    config.driver,
                    if multi {
                        ", one JACK client per plugin"
                    } else {
                        ""
                    }
                );
                self.emit(Event::Started {
                    driver: config.driver,
                    sample_rate: sr,
                    buffer_size: bs,
                });
                if multi {
                    self.connect_at = Some(Instant::now() + CONNECT_DELAY);
                }
            }
            Err(e) => {
                log::error!("engine failed to start with {}: {e}", config.driver);
                self.config = config;
                self.emit(Event::StartFailed(e));
            }
        }
    }

    fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.host.remove_all();
        self.host.close();
        self.running = false;
        self.multi = false;
        self.hardware = None;
        self.slots.clear();
        self.pending_loads.clear();
        self.loading_rig = false;
        self.connect_at = None;
        self.dirty = false;
        while self.raw_rx.try_recv().is_ok() {}
        self.internal.clear();
        self.external.clear();
        self.view = View::Internal;
        self.emit(Event::Stopped);
    }

    /// Handles every queued Carla callback now. Called after each change to
    /// Carla, so callbacks refer to the plugin ids as they are.
    fn drain(&mut self) {
        while let Ok(raw) = self.raw_rx.try_recv() {
            self.on_callback(raw);
        }
    }

    fn mark(&self, action: u32) {
        let _ = self.raw_tx.send(RawCallback {
            action,
            plugin_id: 0,
            value1: 0,
            value2: 0,
            valuef: 0.0,
            text: String::new(),
        });
    }

    fn emit_clients(&self) {
        self.emit(Event::Clients(
            self.external.clients_info(&self.config.client_name),
        ));
    }

    // --------------------------------------------------------------- plugins

    /// The name a Carla plugin gets: unique, so that under JACK its client
    /// can be told apart from every other ("Carlita.12 Big Muff").
    fn carla_name(&self, owner: Owner, load_name: &str) -> String {
        let client = &self.config.client_name;
        match owner {
            Owner::OutputBus => format!("{client}.out"),
            Owner::TrackBus(t) => format!("{client}.track-{}", t.0),
            Owner::TrackInputBus(t) => format!("{client}.in-{}", t.0),
            Owner::InputProbe(k) => format!("{client}.probe-{}", k + 1),
            Owner::Plugin(s) => clean_name(&format!("{client}.{} {load_name}", s.0), 48),
        }
    }

    fn add_raw(&self, load: &PluginLoad) -> Result<(), String> {
        let p = &load.plugin;
        self.host.add_plugin(
            carla::binary_type(p.arch),
            carla::plugin_type(p.format),
            &p.filename,
            &self.carla_name(Owner::Plugin(load.slot), &load.name),
            &p.label,
            p.unique_id,
        )
    }

    fn add_placeholder(&self, name: &str) -> Result<(), String> {
        self.host.add_plugin(
            carla::binary_type(Default::default()),
            sys::PLUGIN_INTERNAL,
            "",
            name,
            PLACEHOLDER_LABEL,
            0,
        )
    }

    fn runtime_info(&self, id: u32) -> PluginRuntimeInfo {
        let hints = self.host.plugin_hints(id);
        let (ai, ao) = self.host.audio_ports(id);
        let (mi, _) = self.host.midi_ports(id);
        PluginRuntimeInfo {
            has_custom_ui: hints & sys::PLUGIN_HAS_CUSTOM_UI != 0,
            can_embed: hints & sys::PLUGIN_HAS_CUSTOM_EMBED_UI != 0,
            can_drywet: hints & sys::PLUGIN_CAN_DRYWET != 0,
            is_bridge: hints & sys::PLUGIN_IS_BRIDGE != 0,
            parameter_count: self.host.parameter_count(id),
            audio_ins: ai,
            audio_outs: ao,
            midi_ins: mi,
        }
    }

    /// Restores state and bypass on a freshly loaded plugin.
    fn prepare_loaded(&self, id: u32, load: &PluginLoad, info: &PluginRuntimeInfo) {
        if info.has_custom_ui {
            self.host.set_ui_title(id, &editor_window_title(load.slot));
        }
        if let Some(state) = &load.state {
            let path = self
                .state_dir
                .path()
                .join(format!("load-{}.carxs", load.slot.0));
            if std::fs::write(&path, state).is_ok() && !self.host.load_state(id, &path) {
                self.emit(Event::Error(format!(
                    "Could not restore the state of {}",
                    load.name
                )));
            }
            let _ = std::fs::remove_file(path);
        }
        self.apply_bypass(id, info, load.bypassed);
    }

    fn apply_bypass(&self, id: u32, info: &PluginRuntimeInfo, bypassed: bool) {
        if info.can_drywet {
            // Fully dry = the input passes straight through, latency aside.
            self.host.set_drywet(id, if bypassed { 0.0 } else { 1.0 });
            self.host.set_active(id, true);
        } else {
            // Plugins whose input and output layouts differ (synths, mono to
            // stereo) cannot pass audio through; deactivating silences them.
            self.host.set_active(id, !bypassed);
        }
    }

    /// Adds an internal gain plugin used as a bus.
    fn add_bus(&mut self, owner: Owner) {
        let name = self.carla_name(owner, "");
        if let Err(e) = self.add_placeholder(&name) {
            log::warn!("could not add the {name} bus: {e}");
            return;
        }
        self.slots.push(Slot {
            owner,
            name,
            load: None,
            placeholder: false,
            info: PluginRuntimeInfo::default(),
            group: None,
        });
        self.drain();
        self.dirty = true;
    }

    /// Adds and removes track buses to match the layout and sets volumes,
    /// and the probes that meter the hardware inputs.
    fn sync_buses(&mut self) {
        if !self.running {
            return;
        }
        let tracks: Vec<(TrackId, f32)> = self
            .layout
            .tracks
            .iter()
            .map(|t| (t.id, t.volume))
            .collect();
        let probes = self.probe_inputs.div_ceil(2);
        while let Some(i) = self.slots.iter().position(|s| match s.owner {
            Owner::TrackBus(t) | Owner::TrackInputBus(t) => !tracks.iter().any(|(id, _)| *id == t),
            Owner::InputProbe(k) => k >= probes,
            _ => false,
        }) {
            self.host.remove_plugin(i as u32);
            self.slots.remove(i);
            self.drain();
            self.dirty = true;
        }
        for (id, volume) in tracks {
            for owner in [Owner::TrackBus(id), Owner::TrackInputBus(id)] {
                if self.slot_of(owner).is_none() {
                    self.add_bus(owner);
                }
            }
            if let Some(i) = self
                .slots
                .iter()
                .position(|s| s.owner == Owner::TrackBus(id))
            {
                self.host
                    .set_parameter(i as u32, GAIN_PARAM, volume.clamp(0.0, 4.0));
            }
        }
        for k in 0..probes {
            if self.slot_of(Owner::InputProbe(k)).is_none() {
                self.add_bus(Owner::InputProbe(k));
            }
        }
    }

    fn set_layout(&mut self, layout: RigLayout) {
        self.layout = layout;
        self.sync_buses();
        self.dirty = true;
    }

    fn add_plugin(&mut self, load: PluginLoad) {
        if self.index_of(load.slot).is_some() {
            return;
        }
        let id = self.host.plugin_count();
        if id as usize != self.slots.len() {
            log::warn!(
                "Carla has {id} plugins but Carlita tracks {}",
                self.slots.len()
            );
        }
        let placeholder = match self.add_raw(&load) {
            Ok(()) => false,
            Err(error) => {
                log::warn!("failed to load {}: {error}", load.name);
                self.emit(Event::PluginFailed {
                    slot: load.slot,
                    error,
                });
                let name = self.carla_name(Owner::Plugin(load.slot), &load.name);
                if let Err(e) = self.add_placeholder(&name) {
                    self.emit(Event::Error(format!(
                        "Could not add a placeholder for {}: {e}",
                        load.name
                    )));
                    self.emit(Event::PluginRemoved {
                        slot: load.slot,
                        state: None,
                    });
                    return;
                }
                true
            }
        };
        let info = if placeholder {
            PluginRuntimeInfo::default()
        } else {
            self.runtime_info(id)
        };
        let slot = load.slot;
        self.slots.push(Slot {
            owner: Owner::Plugin(slot),
            name: self.carla_name(Owner::Plugin(slot), &load.name),
            load: Some(load.clone()),
            placeholder,
            info: info.clone(),
            group: None,
        });
        // Picks up the plugin's group in the internal graph.
        self.drain();
        if !placeholder {
            self.prepare_loaded(id, &load, &info);
            self.emit(Event::PluginLoaded { slot, info });
        }
        self.dirty = true;
    }

    fn save_state_of(&mut self, i: usize) -> Option<String> {
        let slot = &self.slots[i];
        let id = slot.id()?;
        if slot.placeholder {
            return slot.load.as_ref().and_then(|l| l.state.clone());
        }
        let path = self.state_dir.path().join(format!("save-{}.carxs", id.0));
        let s = self
            .host
            .save_state(i as u32, &path)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        let _ = std::fs::remove_file(&path);
        if let (Some(state), Some(load)) = (&s, self.slots[i].load.as_mut()) {
            load.state = Some(state.clone());
        }
        s
    }

    fn remove_plugin(&mut self, slot: SlotId) {
        let mut state = None;
        if let Some(pos) = self.pending_loads.iter().position(|l| l.slot == slot) {
            state = self.pending_loads.remove(pos).and_then(|l| l.state);
        }
        if let Some(i) = self.index_of(slot) {
            state = self.save_state_of(i);
            self.host.show_ui(i as u32, false);
            if self.host.remove_plugin(i as u32) {
                self.slots.remove(i);
                self.drain();
                self.dirty = true;
            } else {
                self.emit(Event::Error(self.host.last_error()));
                return;
            }
        }
        self.emit(Event::PluginRemoved { slot, state });
    }

    fn set_bypass(&mut self, slot: SlotId, bypassed: bool) {
        if let Some(l) = self.pending_loads.iter_mut().find(|l| l.slot == slot) {
            l.bypassed = bypassed;
        }
        if let Some(i) = self.index_of(slot) {
            if let Some(l) = self.slots[i].load.as_mut() {
                l.bypassed = bypassed;
            }
            if !self.slots[i].placeholder {
                self.apply_bypass(i as u32, &self.slots[i].info, bypassed);
                // Plugins that cannot pass audio through are routed around.
                if !self.slots[i].info.can_drywet {
                    self.dirty = true;
                }
            }
        }
        self.emit(Event::BypassChanged { slot, bypassed });
    }

    /// Removes every user plugin; the buses stay.
    fn clear_rig(&mut self) {
        self.pending_loads.clear();
        self.loading_rig = false;
        let mut removed = Vec::new();
        while let Some(i) = self.slots.iter().rposition(|s| s.id().is_some()) {
            let id = self.slots[i].id().expect("plugin");
            if self.running {
                self.host.show_ui(i as u32, false);
                self.host.remove_plugin(i as u32);
            }
            self.slots.remove(i);
            removed.push(id);
        }
        self.drain();
        self.dirty = true;
        for slot in removed.into_iter().rev() {
            self.emit(Event::PluginRemoved { slot, state: None });
        }
    }

    fn reload_plugin(&mut self, load: PluginLoad) {
        let Some(i) = self.index_of(load.slot) else {
            if self.require_running() {
                self.add_plugin(load);
            }
            return;
        };
        let id = i as u32;
        self.host.replace_plugin(id);
        match self.add_raw(&load) {
            Ok(()) => {
                let info = self.runtime_info(id);
                let slot = load.slot;
                self.slots[i] = Slot {
                    owner: Owner::Plugin(slot),
                    name: self.carla_name(Owner::Plugin(slot), &load.name),
                    load: Some(load.clone()),
                    placeholder: false,
                    info: info.clone(),
                    group: None,
                };
                self.drain();
                self.prepare_loaded(id, &load, &info);
                self.emit(Event::PluginLoaded { slot, info });
            }
            Err(error) => {
                // Make sure the placeholder is still in place.
                self.host.replace_plugin(id);
                let _ = self.add_placeholder(&self.slots[i].name.clone());
                self.slots[i].placeholder = true;
                self.slots[i].info = PluginRuntimeInfo::default();
                self.drain();
                self.emit(Event::PluginFailed {
                    slot: load.slot,
                    error,
                });
            }
        }
        self.dirty = true;
    }

    fn capture_states(&mut self) -> Vec<(SlotId, Option<String>)> {
        let mut out = Vec::with_capacity(self.slots.len() + self.pending_loads.len());
        for i in 0..self.slots.len() {
            if let Some(id) = self.slots[i].id() {
                let state = self.save_state_of(i);
                out.push((id, state));
            }
        }
        // Plugins still waiting to load keep the state they came with.
        out.extend(self.pending_loads.iter().map(|l| (l.slot, l.state.clone())));
        out
    }

    fn crash(&mut self, id: u32, message: String) {
        let i = id as usize;
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        let Some(slot_id) = slot.id() else { return };
        if slot.placeholder {
            return;
        }
        let name = slot.name.clone();
        log::error!("plugin {name} crashed: {message}");
        self.host.replace_plugin(id);
        if self.add_placeholder(&name).is_ok() {
            self.slots[i].placeholder = true;
            self.slots[i].info = PluginRuntimeInfo::default();
            self.slots[i].group = None;
        }
        self.dirty = true;
        self.emit(Event::PluginCrashed {
            slot: slot_id,
            message,
        });
        self.drain();
    }

    // ------------------------------------------------------------- callbacks

    fn on_callback(&mut self, cb: RawCallback) {
        if cb.action == MARK_EXTERNAL || cb.action == MARK_INTERNAL {
            self.view = if cb.action == MARK_EXTERNAL {
                View::External
            } else {
                View::Internal
            };
            // A refresh replays the whole graph.
            match self.view {
                View::External => self.external.clear(),
                View::Internal => self.internal.clear(),
            }
            return;
        }
        let slot = self.slots.get(cb.plugin_id as usize).and_then(|s| s.id());
        // Whether the callback is about the graph the rig is wired in.
        let routing = self.view == self.routing_view();
        match cb.action {
            sys::ENGINE_CALLBACK_PLUGIN_UNAVAILABLE => self.crash(cb.plugin_id, cb.text),
            sys::ENGINE_CALLBACK_ERROR => {
                let t = cb.text.to_lowercase();
                let is_crash = t.contains("crash")
                    || t.contains("timed out")
                    || t.contains("timeout")
                    || t.contains("stopped responding");
                if is_crash && slot.is_some() {
                    self.crash(cb.plugin_id, cb.text);
                } else {
                    self.emit(Event::Error(cb.text));
                }
            }
            sys::ENGINE_CALLBACK_INFO => self.emit(Event::Info(cb.text)),
            sys::ENGINE_CALLBACK_UI_STATE_CHANGED => {
                if let Some(slot) = slot {
                    if cb.value1 <= 0 {
                        self.emit(Event::EditorClosed { slot });
                    }
                }
            }
            sys::ENGINE_CALLBACK_EMBED_UI_RESIZED => {
                if let Some(slot) = slot {
                    self.emit(Event::EditorResized {
                        slot,
                        width: cb.value1,
                        height: cb.value2,
                    });
                }
            }
            sys::ENGINE_CALLBACK_PARAMETER_VALUE_CHANGED => {
                if let Some(slot) = slot {
                    if cb.value1 >= 0 {
                        self.emit(Event::ParameterChanged {
                            slot,
                            index: cb.value1 as u32,
                            value: cb.valuef,
                        });
                    }
                }
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_ADDED
            | sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_RENAMED => {
                if routing && cb.action == sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_ADDED {
                    if self.multi {
                        // Plugin ids in JACK's metadata go stale when a
                        // plugin before them is removed; names do not.
                        if let Some(s) = self
                            .slots
                            .iter_mut()
                            .find(|s| client_matches(&cb.text, &s.name))
                        {
                            s.group = Some(cb.plugin_id);
                        }
                    } else if cb.value1 == ICON_PLUGIN {
                        if let Some(s) = self.slots.get_mut(cb.value2 as usize) {
                            s.group = Some(cb.plugin_id);
                        }
                    }
                    self.dirty = true;
                }
                self.graph_mut().clients.insert(cb.plugin_id, cb.text);
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_REMOVED => {
                let g = cb.plugin_id;
                let pb = self.graph_mut();
                pb.clients.remove(&g);
                pb.ports.retain(|(c, _), _| *c != g);
                pb.connections
                    .retain(|_, (og, _, ig, _)| *og != g && *ig != g);
                if routing {
                    for s in self.slots.iter_mut().filter(|s| s.group == Some(g)) {
                        s.group = None;
                    }
                    self.dirty = true;
                }
            }
            sys::ENGINE_CALLBACK_PATCHBAY_PORT_ADDED
            | sys::ENGINE_CALLBACK_PATCHBAY_PORT_CHANGED => {
                self.graph_mut().ports.insert(
                    (cb.plugin_id, cb.value1 as u32),
                    (cb.text, cb.value2 as u32),
                );
                self.dirty |= routing;
            }
            sys::ENGINE_CALLBACK_PATCHBAY_PORT_REMOVED => {
                self.graph_mut()
                    .ports
                    .remove(&(cb.plugin_id, cb.value1 as u32));
                self.dirty |= routing;
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CONNECTION_ADDED => {
                let parts: Vec<u32> = cb.text.split(':').filter_map(|p| p.parse().ok()).collect();
                if let [og, op, ig, ip] = parts[..] {
                    self.graph_mut()
                        .connections
                        .insert(cb.plugin_id, (og, op, ig, ip));
                }
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CONNECTION_REMOVED => {
                self.graph_mut().connections.remove(&cb.plugin_id);
            }
            sys::ENGINE_CALLBACK_ENGINE_STOPPED | sys::ENGINE_CALLBACK_QUIT if self.running => {
                self.running = false;
                self.slots.clear();
                self.internal.clear();
                self.external.clear();
                self.emit(Event::Stopped);
            }
            _ => {}
        }
    }

    /// The graph the rig is wired in.
    fn routing_view(&self) -> View {
        if self.multi {
            View::External
        } else {
            View::Internal
        }
    }

    fn routing(&self) -> &Patchbay {
        match self.routing_view() {
            View::Internal => &self.internal,
            View::External => &self.external,
        }
    }

    fn graph_mut(&mut self) -> &mut Patchbay {
        match self.view {
            View::Internal => &mut self.internal,
            View::External => &mut self.external,
        }
    }

    // --------------------------------------------------------------- routing

    /// The group of one of Carlita's own plugins or buses.
    fn group(&self, owner: Owner) -> Option<u32> {
        self.slot_of(owner).and_then(|s| s.group)
    }

    fn is_own_group(&self, g: u32) -> bool {
        self.slots.iter().any(|s| s.group == Some(g))
    }

    /// Ports of `group` in the routing graph.
    fn ports(&self, group: u32, input: bool, audio: bool) -> Vec<u32> {
        self.routing()
            .ports_of(group, |h| {
                is_input(h) == input && if audio { is_audio(h) } else { is_midi(h) }
            })
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }

    fn endpoints(&self, group: u32, input: bool, audio: bool) -> Vec<(u32, u32)> {
        self.ports(group, input, audio)
            .into_iter()
            .map(|p| (group, p))
            .collect()
    }

    /// The JACK client standing for the input or output "device".
    fn device_client(&self, name: &str) -> Option<u32> {
        self.external
            .client_id(if name.is_empty() { "system" } else { name })
    }

    /// The hardware input channels, in order.
    fn hardware_inputs(&self) -> Vec<(u32, u32)> {
        let group = if self.multi {
            self.device_client(&self.config.input_client)
        } else {
            self.internal.client_id(GROUP_AUDIO_IN)
        };
        group.map_or_else(Vec::new, |g| self.endpoints(g, false, true))
    }

    /// The hardware output channels, in order.
    fn hardware_outputs(&self) -> Vec<(u32, u32)> {
        let group = if self.multi {
            self.device_client(&self.config.output_client)
        } else {
            self.internal.client_id(GROUP_AUDIO_OUT)
        };
        group.map_or_else(Vec::new, |g| self.endpoints(g, true, true))
    }

    /// Hardware MIDI ports that can feed a track, with their full names
    /// ("client:port"). Without one client per plugin, every device comes
    /// in merged through Carla's single MIDI input.
    fn midi_devices(&self) -> Vec<((u32, u32), String)> {
        if !self.multi {
            return Vec::new();
        }
        let pb = &self.external;
        let mut v: Vec<((u32, u32), String)> = pb
            .clients
            .iter()
            .filter(|(_, name)| is_midi_hardware(name))
            .flat_map(|(g, name)| {
                pb.ports_of(*g, |h| is_midi(h) && !is_input(h))
                    .into_iter()
                    .map(move |(p, port)| ((*g, p), format!("{name}:{port}")))
            })
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v
    }

    /// Where a track's MIDI comes from.
    fn midi_sources(&self, track: TrackId) -> Vec<(u32, u32)> {
        if !self.multi {
            return self
                .internal
                .client_id(GROUP_MIDI_IN)
                .map_or_else(Vec::new, |g| self.endpoints(g, false, false));
        }
        if !self.config.auto_connect {
            return Vec::new();
        }
        let Some(t) = self.layout.tracks.iter().find(|t| t.id == track) else {
            return Vec::new();
        };
        self.midi_devices()
            .into_iter()
            .filter(|(_, name)| t.midi.takes(name))
            .map(|(e, _)| e)
            .collect()
    }

    /// The ports a link starts from.
    fn sources(&self, node: Node, sel: PortSel, audio: bool) -> Vec<(u32, u32)> {
        let mut v = match node {
            Node::Input if audio => {
                if self.multi && !self.config.auto_connect {
                    Vec::new()
                } else {
                    self.hardware_inputs()
                }
            }
            Node::MidiInput(t) if !audio => return self.midi_sources(t),
            Node::Track(t) => self
                .group(Owner::TrackBus(t))
                .map_or_else(Vec::new, |g| self.endpoints(g, false, audio)),
            Node::Slot(id) => self
                .group(Owner::Plugin(id))
                .map_or_else(Vec::new, |g| self.endpoints(g, false, audio)),
            _ => Vec::new(),
        };
        if let PortSel::Only(mask) = sel {
            v = v
                .into_iter()
                .enumerate()
                .filter(|(k, _)| *k < 64 && mask & (1 << k) != 0)
                .map(|(_, e)| e)
                .collect();
        }
        v
    }

    /// The ports a link ends at.
    fn sinks(&self, node: Node, audio: bool) -> Vec<(u32, u32)> {
        let of = |owner| {
            self.group(owner)
                .map(|g| self.endpoints(g, true, audio))
                .filter(|v| !v.is_empty())
        };
        match node {
            // Only ever through the output bus: never a hardware input
            // straight to the speakers.
            Node::Output => of(Owner::OutputBus).unwrap_or_default(),
            // A track whose bus is missing plays straight into the output.
            Node::Track(t) => {
                of(Owner::TrackBus(t)).unwrap_or_else(|| self.sinks(Node::Output, audio))
            }
            Node::Slot(id) => of(Owner::Plugin(id)).unwrap_or_default(),
            Node::MidiOutput if !audio && !self.multi => self
                .internal
                .client_id(GROUP_MIDI_OUT)
                .map_or_else(Vec::new, |g| self.endpoints(g, true, false)),
            _ => Vec::new(),
        }
    }

    fn node_io(&self, id: SlotId) -> Option<NodeIo> {
        let s = self.slot_of(Owner::Plugin(id))?;
        // A bypassed plugin that cannot pass audio through is left out, so
        // the signal goes around it.
        if !s.placeholder && s.bypassed() && !s.info.can_drywet {
            return None;
        }
        let g = s.group?;
        Some(NodeIo {
            audio_ins: self.ports(g, true, true).len() as u32,
            audio_outs: self.ports(g, false, true).len() as u32,
            midi_ins: self.ports(g, true, false).len() as u32,
            midi_outs: self.ports(g, false, false).len() as u32,
        })
    }

    /// Port pairs that connect `outs` to `ins`: one to one, a mono output
    /// to every input, several outputs summed into a mono input, and more
    /// outputs than inputs spread over them in turn.
    fn pair<T: Copy>(outs: &[T], ins: &[T]) -> Vec<(T, T)> {
        if outs.is_empty() || ins.is_empty() {
            return Vec::new();
        }
        let n = outs.len().max(ins.len());
        (0..n)
            .map(|k| (outs[k % outs.len()], ins[k % ins.len()]))
            .collect()
    }

    fn desired_connections(&self) -> HashSet<(u32, u32, u32, u32)> {
        let mut out = HashSet::new();
        let audio = |out: &mut HashSet<_>, outs: &[(u32, u32)], ins: &[(u32, u32)]| {
            for ((g1, p1), (g2, p2)) in Self::pair(outs, ins) {
                out.insert((g1, p1, g2, p2));
            }
        };
        // Output bus -> the first hardware outputs (not every one of them,
        // on an interface with eight).
        if self.config.auto_connect || !self.multi {
            if let Some(bus) = self.group(Owner::OutputBus) {
                let outs = self.endpoints(bus, false, true);
                let mut hw = self.hardware_outputs();
                if hw.len() > 1 {
                    hw.truncate(outs.len());
                }
                audio(&mut out, &outs, &hw);
            }
        }
        // Each track's inputs -> its input meter.
        for t in &self.layout.tracks {
            if let Some(g) = self.group(Owner::TrackInputBus(t.id)) {
                let from = self.sources(Node::Input, PortSel::Only(t.input.mask()), true);
                audio(&mut out, &from, &self.endpoints(g, true, true));
            }
        }
        // Hardware inputs 2k and 2k+1 -> the left and right of probe k.
        let hw = self.hardware_inputs();
        for k in 0..self.probe_inputs.div_ceil(2) {
            if let Some(g) = self.group(Owner::InputProbe(k)) {
                let ins = self.endpoints(g, true, true);
                for (side, (g2, p2)) in ins.iter().take(2).enumerate() {
                    if let Some((g1, p1)) = hw.get(2 * k as usize + side) {
                        out.insert((*g1, *p1, *g2, *p2));
                    }
                }
            }
        }
        for link in rig::route(&self.layout, |id| self.node_io(id)) {
            match link.kind {
                LinkKind::Audio => audio(
                    &mut out,
                    &self.sources(link.from, link.sel, true),
                    &self.sinks(link.to, true),
                ),
                LinkKind::Midi => {
                    let from = match link.from {
                        // A plugin's MIDI goes on from its first output.
                        Node::Slot(_) => self
                            .sources(link.from, link.sel, false)
                            .into_iter()
                            .take(1)
                            .collect(),
                        n => self.sources(n, link.sel, false),
                    };
                    if let Some((g2, p2)) = self.sinks(link.to, false).first() {
                        for (g1, p1) in from {
                            out.insert((g1, p1, *g2, *p2));
                        }
                    }
                }
            }
        }
        // The stem recorder: each track's output, and the inputs it takes.
        let recorder = self.recorder_group();
        if let Some(rec) = recorder {
            let ins = self.routing().ports_of(rec, |h| is_audio(h) && is_input(h));
            let named = |prefix: &str| -> Vec<(u32, u32)> {
                let mut v: Vec<(&str, u32)> = ins
                    .iter()
                    .map(|(p, n)| (n.rsplit(':').next().unwrap_or(n), *p))
                    .filter(|(n, _)| n.starts_with(prefix))
                    .collect();
                v.sort();
                v.into_iter().map(|(_, p)| (rec, p)).collect()
            };
            for t in &self.layout.tracks {
                if let Some(bus) = self.group(Owner::TrackBus(t.id)) {
                    let to = named(&format!("t{}_out_", t.id.0));
                    audio(&mut out, &self.endpoints(bus, false, true), &to);
                }
                let to = named(&format!("t{}_dry_", t.id.0));
                let from = self.sources(Node::Input, PortSel::Only(t.input.mask()), true);
                for (f, t) in from.iter().zip(&to) {
                    out.insert((f.0, f.1, t.0, t.1));
                }
            }
        }
        // In the JACK graph, only ever connect Carlita's own clients.
        if self.multi {
            out.retain(|(og, _, ig, _)| {
                self.is_own_group(*og) || self.is_own_group(*ig) || Some(*ig) == recorder
            });
        }
        out
    }

    /// The stem recorder's client in the JACK graph, once it is there.
    fn recorder_group(&self) -> Option<u32> {
        let name = self.recorder.as_deref()?;
        if !self.multi {
            return None;
        }
        self.external
            .clients
            .iter()
            .find(|(_, n)| client_matches(n, name))
            .map(|(id, _)| *id)
    }

    /// Whether Carlita looks after this connection. In the JACK graph, that
    /// is everything going into its own clients and its output going to the
    /// output device; connections other programs make elsewhere (or from
    /// Carlita's outputs, to record them) are left alone.
    fn manages(&self, c: &(u32, u32, u32, u32)) -> bool {
        if !self.multi {
            return true;
        }
        let (og, _, ig, _) = *c;
        self.is_own_group(ig)
            || Some(ig) == self.recorder_group()
            || (self.is_own_group(og) && self.device_client(&self.config.output_client) == Some(ig))
    }

    /// Changes only the connections that differ from the layout. New
    /// connections are made before old ones are removed, so a move never
    /// leaves a gap in the signal.
    fn rewire(&mut self) {
        self.dirty = false;
        if !self.running {
            return;
        }
        let external = self.multi;
        let desired = self.desired_connections();
        let current: HashMap<(u32, u32, u32, u32), u32> = self
            .routing()
            .connections
            .iter()
            .map(|(id, c)| (*c, *id))
            .collect();
        let mut changed = false;
        for c in &desired {
            if !current.contains_key(c) {
                if !self.host.patchbay_connect(external, *c) {
                    log::warn!("could not connect {c:?}: {}", self.host.last_error());
                }
                changed = true;
            }
        }
        for (c, id) in &current {
            if !desired.contains(c) && self.manages(c) {
                self.host.patchbay_disconnect(external, *id);
                changed = true;
            }
        }
        if changed {
            let dirty = self.dirty;
            self.drain();
            // Our own connection changes do not need another pass.
            self.dirty = dirty;
        }
        self.emit_hardware();
    }

    /// Tells the UI which hardware inputs and MIDI devices there are, when
    /// that changed.
    fn emit_hardware(&mut self) {
        let pb = self.routing();
        let inputs: Vec<String> = self
            .hardware_inputs()
            .into_iter()
            .map(|(g, p)| {
                pb.ports
                    .get(&(g, p))
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default()
            })
            .collect();
        let midi: Vec<MidiDevice> = self
            .midi_devices()
            .into_iter()
            .map(|(_, id)| MidiDevice {
                name: midi_label(id.split_once(':').map_or(&id[..], |(_, p)| p)),
                id,
            })
            .collect();
        let hw = (inputs, midi);
        if self.hardware.as_ref() != Some(&hw) {
            self.emit(Event::Hardware {
                inputs: hw.0.clone(),
                midi: hw.1.clone(),
                merged_midi: !self.multi,
            });
            self.hardware = Some(hw);
        }
    }

    // ---------------------------------------------------------------- meters

    fn meters(&self) -> Meters {
        let (dsp_load, xruns) = self.host.runtime_info();
        let peak = |owner: Owner, input: bool| -> [f32; 2] {
            match self.slots.iter().position(|s| s.owner == owner) {
                Some(i) if input => [
                    self.host.input_peak(i as u32, true),
                    self.host.input_peak(i as u32, false),
                ],
                Some(i) => [
                    self.host.output_peak(i as u32, true),
                    self.host.output_peak(i as u32, false),
                ],
                None => [0.0; 2],
            }
        };
        let latencies: Vec<(SlotId, u32)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let id = s.id()?;
                Some((
                    id,
                    if s.placeholder {
                        0
                    } else {
                        self.host.latency(i as u32)
                    },
                ))
            })
            .collect();
        let path_latency = rig::path_latency(&self.layout, |id| {
            latencies
                .iter()
                .find(|(s, _)| *s == id)
                .map_or(0, |(_, l)| *l)
        });
        let track_inputs: Vec<(TrackId, [f32; 2])> = self
            .layout
            .tracks
            .iter()
            .map(|t| (t.id, peak(Owner::TrackInputBus(t.id), true)))
            .collect();
        let hardware_inputs: Vec<f32> = (0..self.probe_inputs)
            .map(|c| peak(Owner::InputProbe(c / 2), true)[(c % 2) as usize])
            .collect();
        Meters {
            input: track_inputs
                .iter()
                .fold([0.0; 2], |m, (_, p)| [m[0].max(p[0]), m[1].max(p[1])]),
            output: peak(Owner::OutputBus, false),
            tracks: self
                .layout
                .tracks
                .iter()
                .map(|t| (t.id, peak(Owner::TrackBus(t.id), false)))
                .collect(),
            track_inputs,
            hardware_inputs,
            dsp_load,
            xruns,
            latencies,
            path_latency,
            sample_rate: self.host.sample_rate(),
            buffer_size: self.host.buffer_size(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_jack_clients_and_ports() {
        assert!(client_matches("Carlita.12 Amp", "Carlita.12 Amp"));
        assert!(client_matches("Carlita.12 Amp-01", "Carlita.12 Amp"));
        assert!(!client_matches("Carlita.12 Amp 2", "Carlita.12 Amp"));
        assert!(!client_matches("Carlita.12 Amp-", "Carlita.12 Amp"));
        assert!(is_own_client("Carlita", "Carlita"));
        assert!(is_own_client("Carlita.track-1", "Carlita"));
        assert!(!is_own_client("Carlitas", "Carlita"));
        assert_eq!(
            midi_label("Arturia KeyStep 32:(capture_0) KeyStep 32 MIDI 1"),
            "KeyStep 32 MIDI 1"
        );
        assert_eq!(
            midi_label("KeyStep [24] (capture): KeyStep MIDI 1"),
            "KeyStep MIDI 1"
        );
        assert_eq!(midi_label("midi_capture_1"), "midi_capture_1");
        assert_eq!(clean_name("Carlita.3 A:B/C", 48), "Carlita.3 A.B.C");
        assert_eq!(
            Engine::pair(&[1, 2], &[10]),
            vec![(1, 10), (2, 10)],
            "a stereo source sums into a mono input"
        );
    }
}
