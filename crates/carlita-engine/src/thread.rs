//! The engine thread: owns the Carla host and runs its idle loop.

use crate::carla::{self, Host, RawCallback};
use crate::types::*;
use carla_sys as sys;
use carlita_core::chain::swap_sequence;
use carlita_core::discovery::default_search_paths;
use carlita_core::{PluginFormat, SlotId};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(16);
const METER_INTERVAL: Duration = Duration::from_millis(33);
const CONNECT_DELAY: Duration = Duration::from_millis(400);
/// Carla internal stereo gain at unity: used as a pass-through placeholder
/// for plugins that failed to load or crashed, so the chain keeps flowing
/// and Carla ids stay aligned with chain positions.
const PLACEHOLDER_LABEL: &str = "audiogain_s";

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

struct Slot {
    id: SlotId,
    load: PluginLoad,
    placeholder: bool,
    info: PluginRuntimeInfo,
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
    _raw_tx: Box<Sender<RawCallback>>,
    running: bool,
    config: EngineConfig,
    slots: Vec<Slot>,
    pending_loads: VecDeque<PluginLoad>,
    loading_chain: bool,
    patchbay: Patchbay,
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
            _raw_tx: raw_tx,
            running: false,
            config: EngineConfig::default(),
            slots: Vec::new(),
            pending_loads: VecDeque::new(),
            loading_chain: false,
            patchbay: Patchbay::default(),
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
        while let Ok(raw) = self.raw_rx.try_recv() {
            self.on_callback(raw);
        }
        if self.connect_at.is_some_and(|t| Instant::now() >= t) {
            self.connect_at = None;
            self.emit(Event::Clients(
                self.patchbay.clients_info(&self.config.client_name),
            ));
            if self.config.auto_connect {
                self.connect_io();
            }
        }
        // Load queued plugins one per tick so the UI keeps updating.
        if let Some(load) = self.pending_loads.pop_front() {
            self.add_plugin(load, None);
            if self.pending_loads.is_empty() && self.loading_chain {
                self.loading_chain = false;
                self.emit(Event::ChainLoaded);
            }
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
            Command::QueryDrivers => self.emit(Event::Drivers(carla::drivers())),
            Command::AddPlugin { load, index } => {
                if self.require_running() {
                    self.add_plugin(load, index)
                }
            }
            Command::RemovePlugin { slot } => self.remove_plugin(slot),
            Command::MovePlugin { from, to } => self.move_plugin(from, to),
            Command::SetBypass { slot, bypassed } => self.set_bypass(slot, bypassed),
            Command::LoadChain { loads } => {
                self.clear_chain();
                if self.require_running() {
                    self.loading_chain = true;
                    self.pending_loads = loads.into();
                    if self.pending_loads.is_empty() {
                        self.loading_chain = false;
                        self.emit(Event::ChainLoaded);
                    }
                }
            }
            Command::ClearChain => self.clear_chain(),
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
                channel,
                note,
                velocity,
            } => {
                let target = match slot {
                    Some(s) => self.index_of(s),
                    None => self
                        .slots
                        .iter()
                        .position(|s| !s.placeholder && s.info.midi_ins > 0),
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
                    self.disconnect_io();
                    self.connect_io();
                }
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
        self.slots.iter().position(|s| s.id == slot)
    }

    // ---------------------------------------------------------------- engine

    fn start(&mut self, config: EngineConfig) {
        if self.running {
            self.stop();
        }
        let h = &self.host;
        h.set_option(
            sys::ENGINE_OPTION_PROCESS_MODE,
            sys::ENGINE_PROCESS_MODE_CONTINUOUS_RACK as i32,
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

        self.patchbay = Patchbay::default();
        match self.host.init(&config.driver, &config.client_name) {
            Ok(()) => {
                self.running = true;
                let (sr, bs) = (self.host.sample_rate(), self.host.buffer_size());
                log::info!("engine started: {} @ {sr} Hz, {bs} frames", config.driver);
                self.config = config.clone();
                self.emit(Event::Started {
                    driver: config.driver,
                    sample_rate: sr,
                    buffer_size: bs,
                });
                if self.config.driver.eq_ignore_ascii_case("jack") {
                    self.host.patchbay_refresh();
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
        self.loading_chain = false;
        while self.raw_rx.try_recv().is_ok() {}
        self.emit(Event::Stopped);
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

    fn add_plugin(&mut self, load: PluginLoad, index: Option<usize>) {
        let before = self.host.plugin_count();
        let (placeholder, info) = match self.add_raw(&load) {
            Ok(()) => {
                let info = self.runtime_info(before);
                self.prepare_loaded(before, &load, &info);
                (false, info)
            }
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
                (true, PluginRuntimeInfo::default())
            }
        };
        let slot = load.slot;
        self.slots.push(Slot {
            id: slot,
            load,
            placeholder,
            info: info.clone(),
        });
        if let Some(index) = index {
            let from = self.slots.len() - 1;
            self.move_plugin(from, index);
        }
        if !placeholder {
            self.emit(Event::PluginLoaded { slot, info });
        }
    }

    fn save_state_of(&mut self, i: usize) -> Option<String> {
        if self.slots[i].placeholder {
            return self.slots[i].load.state.clone();
        }
        let path = self
            .state_dir
            .path()
            .join(format!("save-{}.carxs", self.slots[i].id.0));
        let s = self
            .host
            .save_state(i as u32, &path)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        let _ = std::fs::remove_file(&path);
        if s.is_some() {
            self.slots[i].load.state = s.clone();
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
            } else {
                self.emit(Event::Error(self.host.last_error()));
                return;
            }
        }
        self.emit(Event::PluginRemoved { slot, state });
    }

    fn move_plugin(&mut self, from: usize, to: usize) {
        let n = self.slots.len();
        if from >= n || n == 0 {
            return;
        }
        for (a, b) in swap_sequence(from, to.min(n - 1)) {
            if self.host.switch_plugins(a, b) {
                self.slots.swap(a as usize, b as usize);
            } else {
                self.emit(Event::Error(format!(
                    "Could not reorder plugins: {}",
                    self.host.last_error()
                )));
                return;
            }
        }
    }

    fn set_bypass(&mut self, slot: SlotId, bypassed: bool) {
        if let Some(l) = self.pending_loads.iter_mut().find(|l| l.slot == slot) {
            l.bypassed = bypassed;
        }
        if let Some(i) = self.index_of(slot) {
            self.slots[i].load.bypassed = bypassed;
            if !self.slots[i].placeholder {
                self.apply_bypass(i as u32, &self.slots[i].info, bypassed);
            }
        }
        self.emit(Event::BypassChanged { slot, bypassed });
    }

    fn clear_chain(&mut self) {
        self.pending_loads.clear();
        self.loading_chain = false;
        if self.running && !self.slots.is_empty() {
            for i in 0..self.slots.len() {
                self.host.show_ui(i as u32, false);
            }
            self.host.remove_all();
        }
        for s in std::mem::take(&mut self.slots) {
            self.emit(Event::PluginRemoved {
                slot: s.id,
                state: None,
            });
        }
    }

    fn reload_plugin(&mut self, load: PluginLoad) {
        let Some(i) = self.index_of(load.slot) else {
            if self.require_running() {
                self.add_plugin(load, None);
            }
            return;
        };
        let id = i as u32;
        self.host.replace_plugin(id);
        match self.add_raw(&load) {
            Ok(()) => {
                let info = self.runtime_info(id);
                self.prepare_loaded(id, &load, &info);
                let slot = load.slot;
                self.slots[i] = Slot {
                    id: slot,
                    load,
                    placeholder: false,
                    info: info.clone(),
                };
                self.emit(Event::PluginLoaded { slot, info });
            }
            Err(error) => {
                // Make sure the placeholder is still in place.
                self.host.replace_plugin(id);
                let _ = self.add_placeholder(&load.name);
                self.slots[i].placeholder = true;
                self.emit(Event::PluginFailed {
                    slot: load.slot,
                    error,
                });
            }
        }
    }

    fn capture_states(&mut self) -> Vec<(SlotId, Option<String>)> {
        let mut out = Vec::with_capacity(self.slots.len() + self.pending_loads.len());
        for i in 0..self.slots.len() {
            let state = self.save_state_of(i);
            out.push((self.slots[i].id, state));
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
        if slot.placeholder {
            return;
        }
        let (slot_id, name) = (slot.id, slot.load.name.clone());
        log::error!("plugin {name} crashed: {message}");
        self.host.replace_plugin(id);
        if self.add_placeholder(&name).is_ok() {
            self.slots[i].placeholder = true;
            self.slots[i].info = PluginRuntimeInfo::default();
        }
        self.emit(Event::PluginCrashed {
            slot: slot_id,
            message,
        });
    }

    // ------------------------------------------------------------- callbacks

    fn on_callback(&mut self, cb: RawCallback) {
        let slot = self.slots.get(cb.plugin_id as usize).map(|s| s.id);
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
                self.patchbay.clients.insert(cb.plugin_id, cb.text);
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_REMOVED => {
                self.patchbay.clients.remove(&cb.plugin_id);
                self.patchbay.ports.retain(|(c, _), _| *c != cb.plugin_id);
            }
            sys::ENGINE_CALLBACK_PATCHBAY_PORT_ADDED
            | sys::ENGINE_CALLBACK_PATCHBAY_PORT_CHANGED => {
                self.patchbay.ports.insert(
                    (cb.plugin_id, cb.value1 as u32),
                    (cb.text, cb.value2 as u32),
                );
            }
            sys::ENGINE_CALLBACK_PATCHBAY_PORT_REMOVED => {
                self.patchbay
                    .ports
                    .remove(&(cb.plugin_id, cb.value1 as u32));
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CONNECTION_ADDED => {
                let parts: Vec<u32> = cb.text.split(':').filter_map(|p| p.parse().ok()).collect();
                if let [og, op, ig, ip] = parts[..] {
                    self.patchbay
                        .connections
                        .insert(cb.plugin_id, (og, op, ig, ip));
                }
            }
            sys::ENGINE_CALLBACK_PATCHBAY_CONNECTION_REMOVED => {
                self.patchbay.connections.remove(&cb.plugin_id);
            }
            sys::ENGINE_CALLBACK_ENGINE_STOPPED | sys::ENGINE_CALLBACK_QUIT if self.running => {
                self.running = false;
                self.slots.clear();
                self.emit(Event::Stopped);
            }
            _ => {}
        }
        // Topology changes refresh the device lists in the UI.
        if matches!(
            cb.action,
            sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_ADDED
                | sys::ENGINE_CALLBACK_PATCHBAY_CLIENT_REMOVED
        ) && self.connect_at.is_none()
        {
            self.emit(Event::Clients(
                self.patchbay.clients_info(&self.config.client_name),
            ));
        }
    }

    // ---------------------------------------------------------- connections

    fn connect_io(&mut self) {
        let pb = &self.patchbay;
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
            if !existing.contains(&c) && !self.host.patchbay_connect(c.0, c.1, c.2, c.3) {
                log::warn!("could not connect {c:?}: {}", self.host.last_error());
            }
        }
    }

    fn disconnect_io(&mut self) {
        let Some(own) = self.patchbay.client_id(&self.config.client_name) else {
            return;
        };
        let ids: Vec<u32> = self
            .patchbay
            .connections
            .iter()
            .filter(|(_, (og, op, ig, ip))| {
                let audio = |c: u32, p: u32| {
                    self.patchbay
                        .ports
                        .get(&(c, p))
                        .is_some_and(|(_, h)| is_audio(*h))
                };
                (*og == own && audio(*og, *op)) || (*ig == own && audio(*ig, *ip))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.host.patchbay_disconnect(id);
        }
    }

    // ---------------------------------------------------------------- meters

    fn meters(&self) -> Meters {
        let (dsp_load, xruns) = self.host.runtime_info();
        let n = self.slots.len() as u32;
        let (input, output) = if n == 0 {
            ([0.0; 2], [0.0; 2])
        } else {
            (
                [
                    self.host.input_peak(0, true),
                    self.host.input_peak(0, false),
                ],
                [
                    self.host.output_peak(n - 1, true),
                    self.host.output_peak(n - 1, false),
                ],
            )
        };
        let latencies = self
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    s.id,
                    if s.placeholder {
                        0
                    } else {
                        self.host.latency(i as u32)
                    },
                )
            })
            .collect();
        Meters {
            input,
            output,
            dsp_load,
            xruns,
            latencies,
            sample_rate: self.host.sample_rate(),
            buffer_size: self.host.buffer_size(),
        }
    }
}
