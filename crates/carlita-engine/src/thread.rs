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
    InputBus,
    OutputBus,
    TrackBus(TrackId),
    Plugin(SlotId),
}

/// A Carla plugin. `Engine::slots[i]` is Carla plugin id `i`.
struct Slot {
    owner: Owner,
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
            .filter(|(_, n)| n.as_str() != own)
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
            match rx.recv_timeout(TICK) {
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
            self.with_external(|e| {
                e.emit_clients();
                if e.config.auto_connect {
                    e.connect_io();
                }
            });
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
        if self.last_meters.elapsed() >= METER_INTERVAL {
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
                if self.running {
                    self.with_external(|e| e.emit_clients());
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
            Command::Reconnect {
                input_client,
                output_client,
            } => {
                self.config.input_client = input_client;
                self.config.output_client = output_client;
                if self.running {
                    self.with_external(|e| {
                        e.disconnect_io();
                        e.connect_io();
                    });
                }
            }
            Command::DescribeGraph { reply } => {
                let name = |g: u32| {
                    self.internal
                        .clients
                        .get(&g)
                        .cloned()
                        .unwrap_or_else(|| g.to_string())
                };
                let mut edges: Vec<(String, String)> = self
                    .internal
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
        let h = &self.host;
        h.set_option(
            sys::ENGINE_OPTION_PROCESS_MODE,
            sys::ENGINE_PROCESS_MODE_PATCHBAY as i32,
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
        h.set_option(
            sys::ENGINE_OPTION_FRONTEND_UI_SCALE,
            (config.ui_scale.clamp(0.5, 4.0) * 1000.0).round() as i32,
            None,
        );
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
        match self.host.init(&config.driver, &config.client_name) {
            Ok(()) => {
                self.running = true;
                self.drain();
                self.add_bus(Owner::InputBus);
                self.add_bus(Owner::OutputBus);
                self.sync_buses();
                self.dirty = true;
                let (sr, bs) = (self.host.sample_rate(), self.host.buffer_size());
                log::info!("engine started: {} @ {sr} Hz, {bs} frames", config.driver);
                self.config = config.clone();
                self.emit(Event::Started {
                    driver: config.driver,
                    sample_rate: sr,
                    buffer_size: bs,
                });
                if self.is_jack() {
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

    fn is_jack(&self) -> bool {
        self.config.driver.to_ascii_lowercase().starts_with("jack")
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

    /// Runs `f` with the JACK graph loaded into `self.external`, then
    /// switches Carla's patchbay reports back to the internal graph.
    fn with_external(&mut self, f: impl FnOnce(&mut Self)) {
        if !self.running || !self.is_jack() {
            return;
        }
        self.mark(MARK_EXTERNAL);
        self.host.patchbay_refresh(true);
        self.drain();
        f(self);
        self.mark(MARK_INTERNAL);
        self.host.patchbay_refresh(false);
        self.drain();
    }

    fn emit_clients(&self) {
        self.emit(Event::Clients(
            self.external.clients_info(&self.config.client_name),
        ));
    }

    // --------------------------------------------------------------- plugins

    fn add_raw(&self, load: &PluginLoad) -> Result<(), String> {
        let p = &load.plugin;
        self.host.add_plugin(
            carla::binary_type(p.arch),
            carla::plugin_type(p.format),
            &p.filename,
            &load.name,
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
        let name = match owner {
            Owner::InputBus => "Input".to_string(),
            Owner::OutputBus => "Output".to_string(),
            Owner::TrackBus(t) => format!("Track {}", t.0),
            Owner::Plugin(_) => unreachable!("buses only"),
        };
        if let Err(e) = self.add_placeholder(&name) {
            log::warn!("could not add the {name} bus: {e}");
            return;
        }
        self.slots.push(Slot {
            owner,
            load: None,
            placeholder: false,
            info: PluginRuntimeInfo::default(),
            group: None,
        });
        self.drain();
        self.dirty = true;
    }

    /// Adds and removes track buses to match the layout and sets volumes.
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
        while let Some(i) = self.slots.iter().position(
            |s| matches!(s.owner, Owner::TrackBus(t) if !tracks.iter().any(|(id, _)| *id == t)),
        ) {
            self.host.remove_plugin(i as u32);
            self.slots.remove(i);
            self.drain();
            self.dirty = true;
        }
        for (id, volume) in tracks {
            if self.slot_of(Owner::TrackBus(id)).is_none() {
                self.add_bus(Owner::TrackBus(id));
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
                if let Err(e) = self.add_placeholder(&load.name) {
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
                let _ = self.add_placeholder(&load.name);
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
        let name = slot
            .load
            .as_ref()
            .map(|l| l.name.clone())
            .unwrap_or_default();
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
        let internal = self.view == View::Internal;
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
                if internal
                    && cb.action == sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_ADDED
                    && cb.value1 == ICON_PLUGIN
                {
                    if let Some(s) = self.slots.get_mut(cb.value2 as usize) {
                        s.group = Some(cb.plugin_id);
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
                if internal {
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
                self.dirty |= internal;
            }
            sys::ENGINE_CALLBACK_PATCHBAY_PORT_REMOVED => {
                self.graph_mut()
                    .ports
                    .remove(&(cb.plugin_id, cb.value1 as u32));
                self.dirty |= internal;
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

    fn graph_mut(&mut self) -> &mut Patchbay {
        match self.view {
            View::Internal => &mut self.internal,
            View::External => &mut self.external,
        }
    }

    // ---------------------------------------------------------- connections

    /// Connects the JACK client to the input/output clients and hardware
    /// MIDI. Runs inside `with_external`.
    fn connect_io(&mut self) {
        let pb = &self.external;
        let Some(own) = pb.client_id(&self.config.client_name) else {
            log::warn!("own JACK client not found in patchbay; skipping auto-connect");
            return;
        };
        let pick = |name: &str| -> Option<u32> {
            if name.is_empty() {
                pb.client_id("system")
            } else {
                pb.client_id(name)
            }
        };
        let own_in = pb.ports_of(own, |h| is_audio(h) && is_input(h));
        let own_out = pb.ports_of(own, |h| is_audio(h) && !is_input(h));
        let own_midi_in = pb.ports_of(own, |h| is_midi(h) && is_input(h));

        let mut todo = Vec::new();
        if let Some(src) = pick(&self.config.input_client) {
            let outs = pb.ports_of(src, |h| is_audio(h) && !is_input(h));
            for (k, (dst_port, _)) in own_in.iter().enumerate() {
                // A mono source feeds both chain inputs.
                if let Some((p, _)) = outs.get(k).or(outs.first()) {
                    todo.push((src, *p, own, *dst_port));
                }
            }
        }
        if let Some(dst) = pick(&self.config.output_client) {
            let ins = pb.ports_of(dst, |h| is_audio(h) && is_input(h));
            for ((src_port, _), (p, _)) in own_out.iter().zip(ins.iter()) {
                todo.push((own, *src_port, dst, *p));
            }
        }
        // Hardware MIDI (JACK "system", PipeWire "Midi-Bridge", a2jmidid "a2j").
        if let Some((midi_in, _)) = own_midi_in.first() {
            for (cid, name) in &pb.clients {
                if *cid == own
                    || !(name == "system"
                        || name.starts_with("Midi-Bridge")
                        || name.starts_with("a2j"))
                {
                    continue;
                }
                for (p, _) in pb.ports_of(*cid, |h| is_midi(h) && !is_input(h)) {
                    todo.push((*cid, p, own, *midi_in));
                }
            }
        }
        let existing: Vec<_> = pb.connections.values().copied().collect();
        for c in todo {
            if !existing.contains(&c) && !self.host.patchbay_connect(true, c) {
                log::warn!("could not connect {c:?}: {}", self.host.last_error());
            }
        }
    }

    fn disconnect_io(&mut self) {
        let Some(own) = self.external.client_id(&self.config.client_name) else {
            return;
        };
        let ids: Vec<u32> = self
            .external
            .connections
            .iter()
            .filter(|(_, (og, op, ig, ip))| {
                let audio = |c: u32, p: u32| {
                    self.external
                        .ports
                        .get(&(c, p))
                        .is_some_and(|(_, h)| is_audio(*h))
                };
                (*og == own && audio(*og, *op)) || (*ig == own && audio(*ig, *ip))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.host.patchbay_disconnect(true, id);
        }
    }

    // --------------------------------------------------------------- routing

    /// The internal-graph group that stands for `node`.
    fn group_of(&self, node: Node) -> Option<u32> {
        let bus = |owner| self.slot_of(owner).and_then(|s| s.group);
        match node {
            Node::Input => bus(Owner::InputBus).or_else(|| self.internal.client_id(GROUP_AUDIO_IN)),
            Node::Output => {
                bus(Owner::OutputBus).or_else(|| self.internal.client_id(GROUP_AUDIO_OUT))
            }
            Node::MidiInput => self.internal.client_id(GROUP_MIDI_IN),
            Node::MidiOutput => self.internal.client_id(GROUP_MIDI_OUT),
            Node::Track(t) => bus(Owner::TrackBus(t)),
            Node::Slot(id) => bus(Owner::Plugin(id)),
        }
    }

    fn ports(&self, group: u32, input: bool, audio: bool) -> Vec<u32> {
        self.internal
            .ports_of(group, |h| {
                is_input(h) == input && if audio { is_audio(h) } else { is_midi(h) }
            })
            .into_iter()
            .map(|(p, _)| p)
            .collect()
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
    /// to every input, several outputs summed into a mono input.
    fn pair(outs: &[u32], ins: &[u32]) -> Vec<(u32, u32)> {
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
        let audio = |out: &mut HashSet<_>, from: u32, outs: &[u32], to: u32, ins: &[u32]| {
            for (a, b) in Self::pair(outs, ins) {
                out.insert((from, a, to, b));
            }
        };
        // Hardware in -> input bus, output bus -> hardware out.
        let hw_in = self.internal.client_id(GROUP_AUDIO_IN);
        let hw_out = self.internal.client_id(GROUP_AUDIO_OUT);
        let in_bus = self.slot_of(Owner::InputBus).and_then(|s| s.group);
        let out_bus = self.slot_of(Owner::OutputBus).and_then(|s| s.group);
        if let (Some(hw), Some(bus)) = (hw_in, in_bus) {
            audio(
                &mut out,
                hw,
                &self.ports(hw, false, true),
                bus,
                &self.ports(bus, true, true),
            );
        }
        if let (Some(bus), Some(hw)) = (out_bus, hw_out) {
            audio(
                &mut out,
                bus,
                &self.ports(bus, false, true),
                hw,
                &self.ports(hw, true, true),
            );
        }
        for link in rig::route(&self.layout, |id| self.node_io(id)) {
            let Some(from) = self.group_of(link.from) else {
                continue;
            };
            // A track whose bus is missing plays straight into the output.
            let to = match link.to {
                Node::Track(_) => self
                    .group_of(link.to)
                    .or_else(|| self.group_of(Node::Output)),
                n => self.group_of(n),
            };
            let Some(to) = to else { continue };
            match link.kind {
                LinkKind::Audio => {
                    let mut outs = self.ports(from, false, true);
                    match link.sel {
                        PortSel::All => {}
                        PortSel::Left => outs.truncate(1),
                        PortSel::Right => {
                            if outs.len() > 1 {
                                outs.remove(0);
                            }
                            outs.truncate(1);
                        }
                    }
                    audio(&mut out, from, &outs, to, &self.ports(to, true, true));
                }
                LinkKind::Midi => {
                    let outs = self.ports(from, false, false);
                    let ins = self.ports(to, true, false);
                    if let (Some(a), Some(b)) = (outs.first(), ins.first()) {
                        out.insert((from, *a, to, *b));
                    }
                }
            }
        }
        out
    }

    /// Changes only the connections that differ from the layout. New
    /// connections are made before old ones are removed, so a move never
    /// leaves a gap in the signal.
    fn rewire(&mut self) {
        self.dirty = false;
        if !self.running {
            return;
        }
        let desired = self.desired_connections();
        let current: HashMap<(u32, u32, u32, u32), u32> = self
            .internal
            .connections
            .iter()
            .map(|(id, c)| (*c, *id))
            .collect();
        let mut changed = false;
        for c in &desired {
            if !current.contains_key(c) {
                if !self.host.patchbay_connect(false, *c) {
                    log::warn!("could not connect {c:?}: {}", self.host.last_error());
                }
                changed = true;
            }
        }
        for (c, id) in &current {
            if !desired.contains(c) {
                self.host.patchbay_disconnect(false, *id);
                changed = true;
            }
        }
        if changed {
            let dirty = self.dirty;
            self.drain();
            // Our own connection changes do not need another pass.
            self.dirty = dirty;
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
        Meters {
            input: peak(Owner::InputBus, true),
            output: peak(Owner::OutputBus, false),
            tracks: self
                .layout
                .tracks
                .iter()
                .map(|t| (t.id, peak(Owner::TrackBus(t.id), false)))
                .collect(),
            dsp_load,
            xruns,
            latencies,
            path_latency,
            sample_rate: self.host.sample_rate(),
            buffer_size: self.host.buffer_size(),
        }
    }
}
