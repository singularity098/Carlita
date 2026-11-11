//! Application state and the glue between the UI, the engine, the editor
//! host and the system workers.

use crate::midi_in::MidiListener;
use crate::scan::{self, ScanMsg};
use crate::system::{SysCmd, SysEvent, SystemWorker};
use crate::{
    BtItem, MainWindow, MappingItem, Options, ParamItem, PluginItem, SlotItem, TextUtil, Theme,
    TrackItem, WifiItem,
};
use carlita_core::midi::{self, BypassChange, MidiAction, MidiMapping, MidiMessage};
use carlita_core::paths::AppPaths;
use carlita_core::rig::{self, Location, Row, RowKind, Spot};
use carlita_core::{
    CatalogFilter, ChainSlot, PluginCatalog, PluginCategory, PluginFormat, Preset, PresetStore,
    Rig, Settings, SlotId, TrackId,
};
use carlita_engine::{
    editor_window_title, Command, EngineConfig, EngineHandle, Event, Meters, PluginLoad,
    PluginRuntimeInfo,
};
use carlita_x11::{Area, EditorHost, HostCommand, HostEvent};
use crossbeam_channel::Receiver;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

const UNDO_TIMEOUT: Duration = Duration::from_secs(6);
/// Row heights of the chain list.
const ROW_PLUGIN: f32 = 68.0;
const ROW_PARALLEL: f32 = 52.0;
const ROW_LANE: f32 = 40.0;
const ROW_DRY: f32 = 48.0;
const TOAST_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct StartOptions {
    pub fullscreen: Option<bool>,
    pub skip_default_preset: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
enum SlotStatus {
    #[default]
    Loading,
    Ready,
    Failed(String),
    Crashed(String),
}

#[derive(Default)]
struct SlotRuntime {
    status: SlotStatus,
    info: Option<PluginRuntimeInfo>,
    latency: u32,
}

enum Prompt {
    SaveAs,
    Rename(String),
    WifiPassword(String),
    Search,
    RenameTrack(TrackId),
}

enum Confirm {
    DeletePreset(String),
    DeleteTrack(TrackId),
    Reboot,
    Shutdown,
    Restart,
    Quit,
}

/// The plugin editor shown (or about to be shown) in the main area.
struct EditorView {
    slot: SlotId,
}

struct Undo {
    slot: ChainSlot,
    location: Location,
    at: Instant,
}

pub struct App {
    ui: slint::Weak<MainWindow>,
    paths: AppPaths,
    settings: Settings,
    catalog: PluginCatalog,
    filter: CatalogFilter,
    presets: PresetStore,
    rig: Rig,
    /// The track shown in the chain panel.
    view_track: TrackId,
    /// Rows of the chain panel, as last shown.
    rows: Vec<Row>,
    /// Row a dragged plugin would be dropped on.
    drop_row: Option<usize>,
    /// Where the next plugin picked in the browser goes.
    add_target: Option<Location>,
    /// The plugin last shown for each track, shown again when the track is
    /// picked in the track bar.
    track_selection: HashMap<TrackId, SlotId>,
    runtime: HashMap<SlotId, SlotRuntime>,
    mappings: Vec<MidiMapping>,
    selected: Option<SlotId>,
    editor: Option<EditorView>,
    undo: Option<Undo>,
    engine: EngineHandle,
    engine_running: bool,
    first_start: bool,
    skip_default_preset: bool,
    reload_chain_on_start: bool,
    tried_fallback: bool,
    editor_host: Option<EditorHost>,
    /// Whether editors can be shown inside the main window (X11 only).
    embed_available: bool,
    /// What the editor host was last told about the main area.
    editor_area: Option<(Area, bool)>,
    system: SystemWorker,
    midi: MidiListener,
    learning: Option<(SlotId, MidiAction)>,
    scan: Option<Receiver<ScanMsg>>,
    prompt: Option<Prompt>,
    confirm: Option<Confirm>,
    toast_at: Option<Instant>,
    options_page: i32,
    last_sys_refresh: Instant,
    slots_model: Rc<VecModel<SlotItem>>,
    tracks_model: Rc<VecModel<TrackItem>>,
    meters: Meters,
    restart_requested: bool,
}

/// Runs `f` on the app, deferring it if the app is already borrowed
/// (a UI callback fired while Rust was updating the UI).
fn with_app(app: &Rc<RefCell<App>>, f: impl FnOnce(&mut App) + 'static) {
    match app.try_borrow_mut() {
        Ok(mut a) => f(&mut a),
        Err(_) => {
            let app = app.clone();
            slint::Timer::single_shot(Duration::ZERO, move || f(&mut app.borrow_mut()));
        }
    }
}

/// Shows the theme called `name` (Iris for an unknown name) and returns the
/// name it picked.
fn apply_theme(ui: &MainWindow, name: &str) -> String {
    let theme = ui.global::<Theme>();
    let names = theme.get_names();
    let index = names.iter().position(|n| n == name).unwrap_or(0);
    theme.set_index(index as i32);
    names
        .row_data(index)
        .map(|n| n.to_string())
        .unwrap_or_default()
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn strings(v: impl IntoIterator<Item = impl AsRef<str>>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v.into_iter().map(ss).collect::<Vec<_>>()))
}

fn load_of(s: &ChainSlot) -> PluginLoad {
    PluginLoad {
        slot: s.id,
        plugin: s.plugin.clone(),
        name: s.name.clone(),
        bypassed: s.bypassed,
        state: s.state.clone(),
    }
}

/// "A", "B", … for lane headers.
fn lane_name(lane: usize) -> String {
    let mut n = lane;
    let mut out = String::new();
    loop {
        out.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            return out;
        }
        n = n / 26 - 1;
    }
}

/// A linear gain as decibels.
fn volume_text(v: f32) -> String {
    if v <= 0.0001 {
        "-∞ dB".into()
    } else {
        format!("{:+.1} dB", 20.0 * v.log10())
    }
}

impl App {
    pub fn new(ui: &MainWindow, opts: StartOptions) -> anyhow::Result<Rc<RefCell<Self>>> {
        let paths = AppPaths::resolve();
        let mut settings = Settings::load(&paths.settings_file());
        if let Some(fs) = opts.fullscreen {
            settings.display.fullscreen = fs;
        }
        let catalog = PluginCatalog::load(&paths.plugin_cache_file()).unwrap_or_else(|e| {
            log::warn!("plugin cache unreadable: {e}");
            PluginCatalog::default()
        });
        let presets = PresetStore::new(paths.presets_dir());

        let editor_host = if std::env::var_os("DISPLAY").is_some() {
            Some(EditorHost::spawn())
        } else {
            None
        };

        let mut midi = MidiListener::new();
        midi.listen(&settings.midi.learn_input);

        let slots_model = Rc::new(VecModel::default());
        ui.set_slots(ModelRc::from(slots_model.clone()));
        let tracks_model = Rc::new(VecModel::default());
        ui.set_tracks(ModelRc::from(tracks_model.clone()));
        let rig = Rig::new();
        let view_track = rig.tracks[0].id;

        let app = Rc::new(RefCell::new(App {
            ui: ui.as_weak(),
            paths,
            settings,
            catalog,
            filter: CatalogFilter::default(),
            presets,
            rig,
            view_track,
            rows: Vec::new(),
            drop_row: None,
            add_target: None,
            track_selection: HashMap::new(),
            runtime: HashMap::new(),
            mappings: Vec::new(),
            selected: None,
            editor: None,
            undo: None,
            engine: EngineHandle::spawn(),
            engine_running: false,
            first_start: true,
            skip_default_preset: opts.skip_default_preset,
            reload_chain_on_start: false,
            tried_fallback: false,
            embed_available: editor_host.is_some(),
            editor_area: None,
            editor_host,
            system: SystemWorker::spawn(),
            midi,
            learning: None,
            scan: None,
            prompt: None,
            confirm: None,
            toast_at: None,
            options_page: 0,
            last_sys_refresh: Instant::now(),
            slots_model,
            tracks_model,
            meters: Meters::default(),
            restart_requested: false,
        }));

        Self::wire(ui, &app);
        {
            let mut a = app.borrow_mut();
            a.init_ui(ui);
            a.engine.send(Command::QueryDrivers);
            a.start_engine();
            if a.catalog.plugins.is_empty() {
                a.start_scan();
            }
            a.system.send(SysCmd::RefreshWifi);
            a.system.send(SysCmd::RefreshBt);
        }

        // Poll engine/editor/system events at display rate.
        let timer = slint::Timer::default();
        let poll_app = app.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                if let Ok(mut a) = poll_app.try_borrow_mut() {
                    a.poll();
                }
            },
        );
        // Keep the timer alive for the app's lifetime.
        std::mem::forget(timer);
        Ok(app)
    }

    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("main window alive")
    }

    fn init_ui(&mut self, ui: &MainWindow) {
        let d = &self.settings.display;
        if !d.fullscreen {
            ui.window().set_size(slint::LogicalSize::new(
                d.window_width as f32,
                d.window_height as f32,
            ));
        }
        ui.window().set_fullscreen(d.fullscreen);
        ui.set_strip_width(d.edge_strip_width as f32);
        apply_theme(ui, &d.theme);
        ui.set_browser_formats(strings(PluginFormat::ALL.iter().map(|f| f.label())));
        ui.set_browser_categories(strings(PluginCategory::ALL.iter().map(|c| c.label())));
        ui.set_keyboard_visible(self.settings.show_keyboard);
        ui.set_keyboard_base(48 + 12 * self.settings.midi.keyboard_octave as i32);

        let o = ui.global::<Options>();
        o.set_version(ss(env!("CARGO_PKG_VERSION")));
        o.set_fullscreen(d.fullscreen);
        o.set_keyboard_visible(self.settings.show_keyboard);
        self.refresh_audio_options();
        self.refresh_presets();
        self.refresh_midi_options();
        self.refresh_browser();
        self.refresh_slots();
        self.update_editor_ui();
    }

    // =============================================================== wiring

    fn wire(ui: &MainWindow, app: &Rc<RefCell<App>>) {
        macro_rules! on {
            ($target:expr, $method:ident, |$a:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
                let app = app.clone();
                $target.$method(move |$($arg: $ty),*| {
                    with_app(&app, move |$a: &mut App| { $body; });
                });
            }};
        }

        let tu = ui.global::<TextUtil>();
        tu.on_drop_last(|s| {
            let mut s = s.to_string();
            s.pop();
            ss(s)
        });
        tu.on_mask(|s| ss("•".repeat(s.chars().count())));

        // ---- chain
        on!(ui, on_select_slot, |a, id: i32| a
            .select(Some(SlotId(id as u64))));
        on!(ui, on_toggle_bypass, |a, id: i32, b: bool| a
            .set_bypass(SlotId(id as u64), b));
        on!(ui, on_remove_slot, |a, id: i32| a
            .remove_slot(SlotId(id as u64)));
        on!(ui, on_drag_hover, |a, row: i32, y: f32| a
            .drag_hover(row as usize, y));
        on!(ui, on_drag_drop, |a, row: i32| a.drag_drop(row as usize));
        on!(ui, on_split_slot, |a, id: i32| {
            if a.rig.split(SlotId(id as u64)) {
                a.rig_changed();
            }
        });
        on!(ui, on_add_lane, |a, row: i32| {
            if let Some(r) = a.rows.get(row as usize).copied() {
                if a.rig.add_lane(a.view_track, r.block) {
                    a.rig_changed();
                }
            }
        });
        on!(ui, on_remove_lane, |a, row: i32| a
            .remove_lane(row as usize));
        on!(ui, on_add_to_lane, |a, row: i32| {
            if let Some(Row {
                block,
                lane: Some(lane),
                ..
            }) = a.rows.get(row as usize).copied()
            {
                a.open_browser(Some(Location {
                    track: a.view_track,
                    spot: Spot::Lane {
                        block,
                        lane,
                        index: usize::MAX,
                    },
                }));
            }
        });
        on!(ui, on_undo_remove, |a| a.undo_remove());
        on!(ui, on_open_browser, |a| a.open_browser(None));
        on!(ui, on_select_track, |a, id: i32| a
            .select_track(TrackId(id as u64)));
        on!(ui, on_start_drag, |a| {
            a.with_winit(|w| {
                let _ = w.drag_window();
            });
            a.end_window_gesture();
        });
        on!(ui, on_start_resize, |a, dir: SharedString| {
            use slint::winit_030::winit::window::ResizeDirection as D;
            let d = match dir.as_str() {
                "n" => D::North,
                "s" => D::South,
                "e" => D::East,
                "w" => D::West,
                "ne" => D::NorthEast,
                "nw" => D::NorthWest,
                "se" => D::SouthEast,
                _ => D::SouthWest,
            };
            a.with_winit(|w| {
                let _ = w.drag_resize_window(d);
            });
            a.end_window_gesture();
        });
        on!(ui, on_minimize_window, |a| a
            .ui()
            .window()
            .set_minimized(true));
        on!(ui, on_toggle_maximize, |a| {
            let ui = a.ui();
            let m = !ui.window().is_maximized();
            if m {
                a.remember_window_size();
            }
            ui.window().set_maximized(m);
            ui.set_window_maximized(m);
        });
        // Closing saves the window size and settings in `shutdown`.
        on!(ui, on_close_window, |a| a.quit());
        {
            // Closed by the window manager (Alt+F4 and the like).
            let app = Rc::downgrade(app);
            ui.window().on_close_requested(move || {
                if let Some(app) = app.upgrade() {
                    if let Ok(mut a) = app.try_borrow_mut() {
                        a.release_editors();
                    }
                }
                slint::CloseRequestResponse::HideWindow
            });
        }
        on!(ui, on_add_track, |a| {
            a.view_track = a.rig.add_track(None);
            a.rig_changed();
        });
        on!(ui, on_rename_track, |a| {
            let name = a
                .rig
                .track(a.view_track)
                .map(|t| t.name.clone())
                .unwrap_or_default();
            a.open_prompt(
                Prompt::RenameTrack(a.view_track),
                "Rename track",
                name,
                false,
            );
        });
        on!(ui, on_delete_track, |a| a.delete_track());
        on!(ui, on_cycle_input, |a| {
            if let Some(t) = a.rig.track_mut(a.view_track) {
                t.input = t.input.next();
                a.rig_changed();
            }
        });
        on!(ui, on_toggle_mute, |a| {
            if let Some(t) = a.rig.track_mut(a.view_track) {
                t.muted = !t.muted;
                a.rig_changed();
            }
        });
        on!(ui, on_set_volume, |a, v: f32| {
            if let Some(t) = a.rig.track_mut(a.view_track) {
                t.volume = v.clamp(0.0, 2.0);
                a.rig_changed();
            }
        });
        on!(
            ui,
            on_browser_filter,
            |a, text: SharedString, format: SharedString, category: SharedString, fav: bool| {
                a.filter = CatalogFilter {
                    text: text.to_string(),
                    format: PluginFormat::from_label(&format),
                    category: PluginCategory::from_label(&category),
                    favorites_only: fav,
                };
                a.refresh_browser();
            }
        );
        on!(ui, on_browser_pick, |a, key: SharedString| a
            .add_plugin(&key));
        on!(ui, on_browser_favorite, |a, key: SharedString| {
            a.catalog.toggle_favorite(&key);
            a.save_catalog();
            a.refresh_browser();
        });
        on!(ui, on_rescan, |a| a.start_scan());
        on!(ui, on_browser_keyboard, |a| {
            let text = a.filter.text.clone();
            a.open_prompt(Prompt::Search, "Search plugins", text, false);
        });

        // ---- main area
        on!(ui, on_area_changed, |a| a.place_editor());
        on!(ui, on_set_param, |a, index: i32, value: f32| {
            if let Some(slot) = a.selected {
                a.engine.send(Command::SetParameter {
                    slot,
                    index: index as u32,
                    value,
                });
            }
        });

        // ---- overlays
        on!(ui, on_prompt_done, |a, ok: bool, text: SharedString| a
            .prompt_done(ok, text.to_string()));
        on!(ui, on_confirm_done, |a, ok: bool| a.confirm_done(ok));
        on!(ui, on_piano_note, |a, note: i32, on: bool| {
            let slot = a.selected.filter(|s| {
                a.runtime
                    .get(s)
                    .and_then(|r| r.info.as_ref())
                    .is_some_and(|i| i.midi_ins > 0)
            });
            let channel = a.settings.midi.keyboard_channel;
            a.engine.send(Command::SendNote {
                slot,
                track: Some(a.view_track),
                channel,
                note: note.clamp(0, 127) as u8,
                velocity: if on { 100 } else { 0 },
            });
        });
        on!(ui, on_piano_octave, |a, d: i32| {
            let o = (a.settings.midi.keyboard_octave as i32 + d).clamp(-3, 4);
            a.settings.midi.keyboard_octave = o as i8;
            a.ui().set_keyboard_base(48 + 12 * o);
            a.save_settings();
        });
        on!(ui, on_toggle_fullscreen, |a| {
            let fs = !a.settings.display.fullscreen;
            a.set_fullscreen(fs);
        });
        on!(ui, on_options_page, |a, page: i32| {
            a.options_page = page;
            match page {
                0 => a.engine.send(Command::QueryDrivers),
                2 => a.refresh_midi_options(),
                3 => a.system.send(SysCmd::RefreshWifi),
                4 => a.system.send(SysCmd::RefreshBt),
                _ => {}
            }
        });

        // ---- options: audio
        let o = ui.global::<Options>();
        on!(o, on_set_driver, |a, v: SharedString| {
            a.settings.audio.driver = v.to_string();
            a.settings.audio.device.clear();
            a.restart_engine();
        });
        on!(o, on_set_device, |a, v: SharedString| {
            a.settings.audio.device = v.to_string();
            a.restart_engine();
        });
        on!(o, on_set_input, |a, v: SharedString| {
            a.settings.audio.input_client = if v == "system" {
                String::new()
            } else {
                v.to_string()
            };
            a.reconnect();
        });
        on!(o, on_set_output, |a, v: SharedString| {
            a.settings.audio.output_client = if v == "system" {
                String::new()
            } else {
                v.to_string()
            };
            a.reconnect();
        });
        on!(o, on_set_sample_rate, |a, v: SharedString| {
            if let Ok(sr) = v.parse() {
                a.settings.audio.sample_rate = sr;
                a.restart_engine();
            }
        });
        on!(o, on_set_buffer_size, |a, v: SharedString| {
            if let Ok(bs) = v.parse() {
                a.settings.audio.buffer_size = bs;
                a.restart_engine();
            }
        });
        on!(o, on_set_isolate, |a, v: bool| {
            a.settings.audio.isolate_plugins = v;
            a.restart_engine();
        });
        on!(o, on_restart_engine, |a| a.restart_engine());

        // ---- options: presets
        on!(o, on_save_preset, |a| {
            if let Some(name) = a.settings.current_preset.clone() {
                a.save_preset(&name);
            }
        });
        on!(o, on_save_preset_as, |a| a.open_prompt(
            Prompt::SaveAs,
            "Save preset as",
            a.settings.current_preset.clone().unwrap_or_default(),
            false
        ));
        on!(o, on_load_preset, |a, name: SharedString| a
            .load_preset(&name));
        on!(o, on_rename_preset, |a, name: SharedString| a.open_prompt(
            Prompt::Rename(name.to_string()),
            "Rename preset",
            name.to_string(),
            false
        ));
        on!(o, on_delete_preset, |a, name: SharedString| a.open_confirm(
            Confirm::DeletePreset(name.to_string()),
            format!("Delete the preset \"{name}\"?")
        ));
        on!(o, on_set_default_preset, |a, name: SharedString| {
            a.settings.default_preset = (!name.is_empty()).then(|| name.to_string());
            a.save_settings();
            a.refresh_presets();
        });

        // ---- options: midi
        on!(o, on_set_midi_port, |a, v: SharedString| {
            let port = if v == "All inputs" {
                String::new()
            } else {
                v.to_string()
            };
            a.settings.midi.learn_input = port.clone();
            a.midi.listen(&port);
            a.save_settings();
            a.refresh_midi_options();
        });
        on!(o, on_start_learn, |a, action: i32| {
            if let Some(slot) = a.selected {
                a.learning = Some((
                    slot,
                    if action == 1 {
                        MidiAction::Follow
                    } else {
                        MidiAction::Toggle
                    },
                ));
                a.refresh_midi_options();
            }
        });
        on!(o, on_cancel_learn, |a| {
            a.learning = None;
            a.refresh_midi_options();
        });
        on!(o, on_delete_mapping, |a, index: i32| {
            if (index as usize) < a.mappings.len() {
                a.mappings.remove(index as usize);
                a.refresh_midi_options();
                a.refresh_slots();
            }
        });
        on!(o, on_set_keyboard, |a, v: bool| {
            a.settings.show_keyboard = v;
            a.ui().set_keyboard_visible(v);
            a.ui().global::<Options>().set_keyboard_visible(v);
            a.save_settings();
        });

        // ---- options: wifi / bluetooth
        on!(o, on_set_wifi, |a, v: bool| a
            .system
            .send(SysCmd::SetWifi(v)));
        on!(o, on_scan_wifi, |a| a.system.send(SysCmd::ScanWifi));
        on!(o, on_connect_wifi, |a, ssid: SharedString| a
            .connect_wifi(ssid.to_string()));
        on!(o, on_disconnect_wifi, |a| a
            .system
            .send(SysCmd::DisconnectWifi));
        on!(o, on_set_bt, |a, v: bool| a.system.send(SysCmd::SetBt(v)));
        on!(o, on_scan_bt, |a, v: bool| a.system.send(SysCmd::ScanBt(v)));
        on!(o, on_connect_bt, |a, p: SharedString| a
            .system
            .send(SysCmd::ConnectBt(p.to_string())));
        on!(o, on_disconnect_bt, |a, p: SharedString| a
            .system
            .send(SysCmd::DisconnectBt(p.to_string())));
        on!(o, on_forget_bt, |a, p: SharedString| a
            .system
            .send(SysCmd::ForgetBt(p.to_string())));

        // ---- options: display
        on!(o, on_set_fullscreen, |a, v: bool| a.set_fullscreen(v));
        on!(o, on_set_theme, |a, name: SharedString| {
            a.settings.display.theme = apply_theme(&a.ui(), &name);
            a.save_settings();
        });
        on!(o, on_set_brightness, |a, v: f32| {
            a.settings.display.brightness = Some(v.round() as u32);
            a.system.send(SysCmd::SetBrightness(v.round() as u32));
            a.ui().global::<Options>().set_brightness(v);
            a.save_settings();
        });

        // ---- options: system
        on!(o, on_rescan_plugins, |a| a.start_scan());
        on!(o, on_restart_app, |a| a.open_confirm(
            Confirm::Restart,
            "Restart Carlita? Audio stops for a few seconds.".into()
        ));
        on!(o, on_quit, |a| a
            .open_confirm(Confirm::Quit, "Quit Carlita?".into()));
        on!(o, on_reboot, |a| a.open_confirm(
            Confirm::Reboot,
            "Reboot the computer?".into()
        ));
        on!(o, on_shutdown, |a| a.open_confirm(
            Confirm::Shutdown,
            "Shut down the computer?".into()
        ));
    }

    // ============================================================== polling

    fn poll(&mut self) {
        while let Ok(e) = self.engine.events().try_recv() {
            self.on_engine_event(e);
        }
        let host_events: Vec<HostEvent> = self
            .editor_host
            .as_ref()
            .map(|h| h.events().try_iter().collect())
            .unwrap_or_default();
        for e in host_events {
            self.on_host_event(e);
        }
        while let Ok(e) = self.system.events.try_recv() {
            self.on_system_event(e);
        }
        while let Ok(bytes) = self.midi.messages.try_recv() {
            if let Some(msg) = MidiMessage::parse(&bytes) {
                self.on_midi(msg);
            }
        }
        if let Some(rx) = &self.scan {
            let msgs: Vec<ScanMsg> = rx.try_iter().collect();
            for m in msgs {
                self.on_scan(m);
            }
        }
        if self
            .undo
            .as_ref()
            .is_some_and(|u| u.at.elapsed() > UNDO_TIMEOUT)
        {
            self.undo = None;
            self.ui().set_undo_text(ss(""));
        }
        // The window manager can maximize or restore the window too.
        let ui = self.ui();
        let maximized = ui.window().is_maximized();
        if ui.get_window_maximized() != maximized {
            ui.set_window_maximized(maximized);
        }
        if self.toast_at.is_some_and(|t| t.elapsed() > TOAST_TIMEOUT) {
            self.toast_at = None;
            self.ui().set_toast(ss(""));
        }
        if (self.options_page == 3 || self.options_page == 4)
            && self.last_sys_refresh.elapsed() > Duration::from_secs(4)
        {
            self.last_sys_refresh = Instant::now();
            if self.ui().get_right_pos() > 0.0 {
                self.system.send(if self.options_page == 3 {
                    SysCmd::RefreshWifi
                } else {
                    SysCmd::RefreshBt
                });
            }
        }
    }

    /// After a move or resize is handed to the window manager, it gets the
    /// button release, not Slint. Without one, Slint keeps the pointer
    /// grabbed by the title bar: the cursor stays a move cursor and the next
    /// clicks on the window buttons are lost. So release it here.
    fn end_window_gesture(&self) {
        let ui = self.ui.clone();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = ui.upgrade() {
                use slint::platform::{PointerEventButton, WindowEvent};
                let position = slint::LogicalPosition::new(-1.0, -1.0);
                let w = ui.window();
                w.dispatch_event(WindowEvent::PointerReleased {
                    position,
                    button: PointerEventButton::Left,
                });
                w.dispatch_event(WindowEvent::PointerExited);
            }
        });
    }

    fn with_winit(&self, f: impl FnOnce(&slint::winit_030::winit::window::Window)) {
        use slint::winit_030::WinitWindowAccessor;
        self.ui().window().with_winit_window(f);
    }

    /// Shows a track: its chain in the panel and the plugin last shown for
    /// it in the main area.
    fn select_track(&mut self, id: TrackId) {
        if self.rig.track(id).is_none() {
            return;
        }
        self.view_track = id;
        let slot = self
            .track_selection
            .get(&id)
            .copied()
            .filter(|s| self.rig.track_of(*s) == Some(id));
        self.select(slot);
        self.refresh_slots();
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let ui = self.ui();
        ui.set_toast(ss(text.into()));
        ui.set_toast_error(error);
        self.toast_at = Some(Instant::now());
    }

    // =============================================================== engine

    fn engine_config(&self) -> EngineConfig {
        let a = &self.settings.audio;
        EngineConfig {
            driver: a.driver.clone(),
            device: a.device.clone(),
            sample_rate: a.sample_rate,
            buffer_size: a.buffer_size,
            isolate_plugins: a.isolate_plugins,
            input_client: a.input_client.clone(),
            output_client: a.output_client.clone(),
            auto_connect: a.auto_connect,
            client_name: "Carlita".into(),
        }
    }

    fn start_engine(&mut self) {
        let cfg = self.engine_config();
        self.ui()
            .global::<Options>()
            .set_engine_status(ss(format!("Starting {}…", cfg.driver)));
        self.engine.send(Command::Start(cfg));
    }

    /// Restarts the engine with new settings, keeping the chain and its state.
    fn restart_engine(&mut self) {
        self.save_settings();
        self.capture_states();
        self.hide_editor();
        self.reload_chain_on_start = true;
        for r in self.runtime.values_mut() {
            r.status = SlotStatus::Loading;
        }
        self.refresh_slots();
        self.start_engine();
        self.refresh_audio_options();
    }

    fn reconnect(&mut self) {
        self.save_settings();
        self.engine.send(Command::Reconnect {
            input_client: self.settings.audio.input_client.clone(),
            output_client: self.settings.audio.output_client.clone(),
        });
        self.refresh_audio_options();
    }

    fn loads(&self) -> Vec<PluginLoad> {
        self.rig.slots().map(load_of).collect()
    }

    fn load_rig(&self) {
        self.engine.send(Command::LoadRig {
            loads: self.loads(),
            layout: self.rig.layout(),
        });
    }

    /// Sends the rig's structure to the engine and redraws the chain.
    fn rig_changed(&mut self) {
        self.engine.send(Command::SetLayout(self.rig.layout()));
        self.refresh_slots();
    }

    /// Pulls every plugin's current state into the chain (blocking, bounded).
    fn capture_states(&mut self) {
        if !self.engine_running || self.rig.is_empty() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.engine.send(Command::CaptureStates { reply: tx });
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(states) => {
                for (id, state) in states {
                    if let (Some(slot), Some(state)) = (self.rig.get_mut(id), state) {
                        slot.state = Some(state);
                    }
                }
            }
            Err(_) => log::warn!("timed out capturing plugin states"),
        }
    }

    fn on_engine_event(&mut self, e: Event) {
        match e {
            Event::Started {
                driver,
                sample_rate,
                buffer_size,
            } => {
                self.engine_running = true;
                let ui = self.ui();
                let o = ui.global::<Options>();
                o.set_engine_running(true);
                o.set_engine_status(ss(format!(
                    "{driver} · {sample_rate:.0} Hz · {buffer_size} frames"
                )));
                if self.first_start {
                    self.first_start = false;
                    let default = self
                        .settings
                        .default_preset
                        .clone()
                        .filter(|_| !self.skip_default_preset);
                    match default {
                        Some(name) if self.presets.exists(&name) => self.load_preset(&name),
                        _ => {}
                    }
                } else if self.reload_chain_on_start {
                    self.reload_chain_on_start = false;
                    self.load_rig();
                } else {
                    self.engine.send(Command::SetLayout(self.rig.layout()));
                }
                self.engine.send(Command::QueryDrivers);
            }
            Event::StartFailed(err) => {
                self.engine_running = false;
                let ui = self.ui();
                let o = ui.global::<Options>();
                o.set_engine_running(false);
                o.set_engine_status(ss(format!("Audio engine stopped: {err}")));
                // On first launch, fall back from JACK to ALSA so the app is
                // playable even without a JACK/PipeWire server.
                if self.first_start
                    && !self.tried_fallback
                    && self.settings.audio.driver.eq_ignore_ascii_case("jack")
                {
                    self.tried_fallback = true;
                    self.toast("JACK/PipeWire is not running – trying ALSA", true);
                    let mut cfg = self.engine_config();
                    cfg.driver = "ALSA".into();
                    self.engine.send(Command::Start(cfg));
                } else {
                    self.toast(format!("Audio engine failed to start: {err}"), true);
                    self.set_editor_message(
                        "Audio engine is not running",
                        &format!("{err}\n\nOpen options → Audio to pick another driver."),
                    );
                }
            }
            Event::Stopped => {
                self.engine_running = false;
                self.ui().global::<Options>().set_engine_running(false);
            }
            Event::Drivers(drivers) => {
                let ui = self.ui();
                let o = ui.global::<Options>();
                o.set_drivers(strings(drivers.iter().map(|d| d.name.clone())));
                let devices = drivers
                    .iter()
                    .find(|d| d.name == self.settings.audio.driver)
                    .filter(|d| !d.name.to_lowercase().starts_with("jack"))
                    .map(|d| d.devices.clone())
                    .unwrap_or_default();
                o.set_devices(strings(devices));
            }
            Event::Clients(clients) => {
                let ui = self.ui();
                let o = ui.global::<Options>();
                let ins: Vec<String> = clients
                    .iter()
                    .filter(|c| c.audio_outs > 0)
                    .map(|c| c.name.clone())
                    .collect();
                let outs: Vec<String> = clients
                    .iter()
                    .filter(|c| c.audio_ins > 0)
                    .map(|c| c.name.clone())
                    .collect();
                o.set_inputs(strings(ins));
                o.set_outputs(strings(outs));
            }
            Event::PluginLoaded { slot, info } => {
                let has_ui = info.has_custom_ui;
                let r = self.runtime.entry(slot).or_default();
                r.status = SlotStatus::Ready;
                r.info = Some(info);
                self.refresh_slots();
                if self.selected == Some(slot) {
                    self.show_editor(slot);
                } else if self.selected.is_none() && has_ui {
                    // Show a freshly added plugin straight away.
                    self.select(Some(slot));
                }
            }
            Event::PluginFailed { slot, error } => {
                self.runtime.entry(slot).or_default().status = SlotStatus::Failed(error.clone());
                let name = self
                    .rig
                    .get(slot)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                self.toast(format!("{name} failed to load: {error}"), true);
                self.refresh_slots();
            }
            Event::PluginCrashed { slot, message } => {
                self.runtime.entry(slot).or_default().status = SlotStatus::Crashed(message);
                let name = self
                    .rig
                    .get(slot)
                    .map(|s| s.name.clone())
                    .unwrap_or_default();
                self.toast(
                    format!(
                        "{name} crashed. The rest of the chain keeps playing; tap it to reload."
                    ),
                    true,
                );
                if self.selected == Some(slot) {
                    self.hide_editor();
                    self.set_editor_message(
                        &format!("{name} crashed"),
                        "Tap it in the chain to reload it.",
                    );
                }
                self.refresh_slots();
            }
            Event::PluginRemoved { slot, state } => {
                if let Some(u) = self.undo.as_mut().filter(|u| u.slot.id == slot) {
                    if state.is_some() {
                        u.slot.state = state;
                    }
                }
            }
            Event::BypassChanged { .. } => {}
            Event::EditorClosed { slot } | Event::EditorResized { slot, .. }
                if self.selected != Some(slot) => {}
            Event::EditorClosed { slot } => {
                log::info!("editor of slot {} closed", slot.0);
            }
            Event::EditorResized { .. } => {}
            Event::Parameters { slot, params } => {
                if self.selected == Some(slot) {
                    let items: Vec<ParamItem> = params
                        .into_iter()
                        .filter(|p| !p.is_output)
                        .map(|p| ParamItem {
                            index: p.index as i32,
                            name: ss(&p.name),
                            value: p.value,
                            minimum: p.min,
                            maximum: p.max,
                            text: ss(if p.unit.is_empty() || p.text.ends_with(&p.unit) {
                                p.text.clone()
                            } else {
                                format!("{} {}", p.text, p.unit)
                            }),
                            is_bool: p.is_boolean,
                            is_int: p.is_integer,
                        })
                        .collect();
                    self.ui().set_params(ModelRc::new(VecModel::from(items)));
                }
            }
            Event::ParameterChanged { slot, index, value } => {
                if self.selected == Some(slot) {
                    let ui = self.ui();
                    let params = ui.get_params();
                    for i in 0..params.row_count() {
                        if let Some(mut p) = params.row_data(i) {
                            if p.index == index as i32 && (p.value - value).abs() > f32::EPSILON {
                                p.value = value;
                                params.set_row_data(i, p);
                            }
                        }
                    }
                }
            }
            Event::RigLoaded => {
                self.refresh_slots();
            }
            Event::Meters(m) => self.on_meters(m),
            Event::Info(msg) => log::info!("engine: {msg}"),
            Event::Error(msg) => self.toast(msg, true),
        }
    }

    fn on_meters(&mut self, m: Meters) {
        let ui = self.ui();
        ui.set_in_left(m.input[0]);
        ui.set_in_right(m.input[1]);
        ui.set_out_left(m.output[0]);
        ui.set_out_right(m.output[1]);
        let load = format!("{:.0}%", m.dsp_load);
        let latency = format!("{:.1} ms", m.total_latency_ms());
        ui.set_dsp_load(ss(&load));
        ui.set_latency(ss(&latency));
        if m.xruns != self.meters.xruns
            || (m.dsp_load - self.meters.dsp_load).abs() >= 1.0
            || m.latencies != self.meters.latencies
        {
            let o = ui.global::<Options>();
            o.set_dsp_load(ss(&load));
            o.set_latency(ss(&latency));
            o.set_xruns(m.xruns as i32);
        }
        let level = m
            .tracks
            .iter()
            .find(|(t, _)| *t == self.view_track)
            .map_or([0.0; 2], |(_, l)| *l);
        ui.set_track_left(level[0]);
        ui.set_track_right(level[1]);
        // Level bars on the track tabs; only redraw tabs whose level moved.
        for i in 0..self.tracks_model.row_count() {
            let Some(mut item) = self.tracks_model.row_data(i) else {
                continue;
            };
            let l = m
                .tracks
                .iter()
                .find(|(t, _)| t.0 as i32 == item.id)
                .map_or(0.0, |(_, l)| l[0].max(l[1]));
            if (item.level - l).abs() > 0.02 {
                item.level = l;
                self.tracks_model.set_row_data(i, item);
            }
        }
        let mut changed = false;
        for (slot, lat) in &m.latencies {
            if let Some(r) = self.runtime.get_mut(slot) {
                if r.latency != *lat {
                    r.latency = *lat;
                    changed = true;
                }
            }
        }
        self.meters = m;
        if changed {
            self.refresh_slots();
        }
    }

    // ================================================================ chain

    fn slot_item(&self, s: &ChainSlot, row: &Row) -> SlotItem {
        let r = self.runtime.get(&s.id);
        let (status, detail) = match r.map(|r| &r.status) {
            Some(SlotStatus::Ready) => ("", String::new()),
            Some(SlotStatus::Failed(e)) => ("failed", e.clone()),
            Some(SlotStatus::Crashed(e)) => ("crashed", e.clone()),
            _ => ("loading", String::new()),
        };
        SlotItem {
            id: s.id.0 as i32,
            kind: ss("plugin"),
            depth: row.lane.is_some() as i32,
            label: ss(""),
            y: 0.0,
            h: ROW_PLUGIN,
            can_split: row.lane.is_none(),
            name: ss(&s.name),
            format: ss(if s.plugin.yabridge {
                format!("{} · yabridge", s.plugin.format)
            } else {
                s.plugin.format.to_string()
            }),
            bypassed: s.bypassed,
            latency: ss(format!("{} smp", r.map_or(0, |r| r.latency))),
            status: ss(status),
            status_detail: ss(detail),
            selected: self.selected == Some(s.id),
            midi_learned: self.mappings.iter().any(|m| m.slot == s.id),
        }
    }

    fn structure_item(kind: &str, label: String, h: f32, depth: i32) -> SlotItem {
        SlotItem {
            id: -1,
            kind: ss(kind),
            depth,
            label: ss(label),
            h,
            ..Default::default()
        }
    }

    fn row_items(&self) -> Vec<SlotItem> {
        let Some(track) = self.rig.track(self.view_track) else {
            return Vec::new();
        };
        let mut y = 0.0;
        self.rows
            .iter()
            .map(|row| {
                let mut item = match row.kind {
                    RowKind::Plugin => {
                        let slot = row.slot.and_then(|id| self.rig.get(id));
                        match slot {
                            Some(s) => self.slot_item(s, row),
                            None => Self::structure_item("dry", String::new(), ROW_DRY, 1),
                        }
                    }
                    RowKind::Parallel => {
                        let lanes = match track.blocks.get(row.block) {
                            Some(rig::Block::Parallel(p)) => p.lanes.len(),
                            _ => 0,
                        };
                        Self::structure_item(
                            "parallel",
                            format!("Parallel · {lanes} lanes, mixed"),
                            ROW_PARALLEL,
                            0,
                        )
                    }
                    RowKind::Lane => Self::structure_item(
                        "lane",
                        format!("Lane {}", lane_name(row.lane.unwrap_or(0))),
                        ROW_LANE,
                        1,
                    ),
                    RowKind::Dry => Self::structure_item("dry", String::new(), ROW_DRY, 1),
                };
                item.y = y;
                y += item.h;
                item
            })
            .collect()
    }

    fn refresh_slots(&mut self) {
        if self.rig.track(self.view_track).is_none() {
            self.view_track = self.rig.ensure_track();
        }
        self.rows = self
            .rig
            .track(self.view_track)
            .map(rig::rows)
            .unwrap_or_default();
        let items = self.row_items();
        if items.len() == self.slots_model.row_count() {
            for (i, item) in items.into_iter().enumerate() {
                if self.slots_model.row_data(i).as_ref() != Some(&item) {
                    self.slots_model.set_row_data(i, item);
                }
            }
        } else {
            self.slots_model.set_vec(items);
        }
        let tracks: Vec<TrackItem> = self
            .rig
            .tracks
            .iter()
            .map(|t| TrackItem {
                id: t.id.0 as i32,
                name: ss(&t.name),
                selected: t.id == self.view_track,
                muted: t.muted,
                level: self.track_level(t.id),
            })
            .collect();
        if tracks.len() == self.tracks_model.row_count() {
            for (i, item) in tracks.into_iter().enumerate() {
                if self.tracks_model.row_data(i).as_ref() != Some(&item) {
                    self.tracks_model.set_row_data(i, item);
                }
            }
        } else {
            self.tracks_model.set_vec(tracks);
        }
        let ui = self.ui();
        if let Some(t) = self.rig.track(self.view_track) {
            ui.set_track_name(ss(&t.name));
            ui.set_track_input(ss(t.input.label()));
            ui.set_track_muted(t.muted);
            ui.set_track_volume(t.volume);
            ui.set_track_volume_text(ss(volume_text(t.volume)));
        }
        ui.set_preset_name(ss(self.settings.current_preset.clone().unwrap_or_default()));
    }

    fn track_level(&self, id: TrackId) -> f32 {
        self.meters
            .tracks
            .iter()
            .find(|(t, _)| *t == id)
            .map_or(0.0, |(_, l)| l[0].max(l[1]))
    }

    fn open_browser(&mut self, target: Option<Location>) {
        self.add_target = target;
        self.refresh_browser();
        self.ui().set_browser_open(true);
    }

    fn add_plugin(&mut self, key: &str) {
        let Some(desc) = self.catalog.find(key).cloned() else {
            return;
        };
        self.ui().set_browser_open(false);
        if !self.engine_running {
            self.toast("The audio engine is not running", true);
            return;
        }
        let slot = self.rig.new_slot(desc);
        let load = load_of(&slot);
        self.runtime.insert(slot.id, SlotRuntime::default());
        match self.add_target.take() {
            Some(loc) => {
                self.rig.insert(loc, slot);
            }
            None => {
                self.rig.push(self.view_track, slot);
            }
        }
        self.engine.send(Command::AddPlugin { load });
        self.rig_changed();
    }

    fn remove_slot(&mut self, id: SlotId) {
        if self.selected == Some(id) {
            self.select(None);
        }
        let Some((location, slot)) = self.rig.take(id) else {
            return;
        };
        self.runtime.remove(&id);
        self.mappings.retain(|m| m.slot != id);
        self.engine.send(Command::RemovePlugin { slot: id });
        self.ui()
            .set_undo_text(ss(format!("Removed {}", slot.name)));
        self.undo = Some(Undo {
            slot,
            location,
            at: Instant::now(),
        });
        self.rig_changed();
        self.refresh_midi_options();
    }

    fn undo_remove(&mut self) {
        let Some(u) = self.undo.take() else { return };
        self.ui().set_undo_text(ss(""));
        let load = load_of(&u.slot);
        self.runtime.insert(u.slot.id, SlotRuntime::default());
        let loc = self.rig.insert(u.location, u.slot);
        self.view_track = loc.track;
        self.engine.send(Command::AddPlugin { load });
        self.rig_changed();
    }

    /// Row the dragged row's centre (at `y` in the list) is over.
    fn row_at(&self, y: f32) -> usize {
        let mut top = 0.0;
        for (i, item) in self.row_items().iter().enumerate() {
            if y < top + item.h {
                return i;
            }
            top += item.h;
        }
        self.rows.len()
    }

    fn drag_hover(&mut self, from: usize, y: f32) {
        let to = self.row_at(y);
        let items = self.row_items();
        let valid = self
            .rig
            .track(self.view_track)
            .and_then(|t| rig::drop_spot(t, &self.rows, from, to))
            .is_some();
        self.drop_row = valid.then_some(to);
        // The indicator sits at the edge of the target row that the plugin
        // ends up next to.
        let indicator = if !valid {
            -1.0
        } else if to >= items.len() {
            items.last().map_or(0.0, |l| l.y + l.h)
        } else if to > from {
            items[to].y + items[to].h
        } else {
            items[to].y
        };
        self.ui().set_drop_y(indicator);
    }

    fn drag_drop(&mut self, from: usize) {
        let to = self.drop_row.take();
        self.ui().set_drop_y(-1.0);
        let Some(to) = to else { return };
        let Some(track) = self.rig.track(self.view_track) else {
            return;
        };
        let Some(spot) = rig::drop_spot(track, &self.rows, from, to) else {
            return;
        };
        let Some(id) = self.rows.get(from).and_then(|r| r.slot) else {
            return;
        };
        if self.rig.move_slot(
            id,
            Location {
                track: self.view_track,
                spot,
            },
        ) {
            self.rig_changed();
        }
    }

    fn remove_lane(&mut self, row: usize) {
        let Some(Row {
            block,
            lane: Some(lane),
            ..
        }) = self.rows.get(row).copied()
        else {
            return;
        };
        let Some(removed) = self.rig.remove_lane(self.view_track, block, lane) else {
            return;
        };
        for s in &removed {
            if self.selected == Some(s.id) {
                self.select(None);
            }
            self.runtime.remove(&s.id);
            self.mappings.retain(|m| m.slot != s.id);
            self.engine.send(Command::RemovePlugin { slot: s.id });
        }
        if !removed.is_empty() {
            self.toast(
                format!(
                    "Removed lane {} and {} plugin(s)",
                    lane_name(lane),
                    removed.len()
                ),
                false,
            );
        }
        self.rig_changed();
        self.refresh_midi_options();
    }

    fn delete_track(&mut self) {
        let Some(t) = self.rig.track(self.view_track) else {
            return;
        };
        if self.rig.tracks.len() == 1 && t.is_empty() {
            return;
        }
        let text = if t.is_empty() {
            format!("Delete the track \"{}\"?", t.name)
        } else {
            format!(
                "Delete the track \"{}\" and its {} plugin(s)?",
                t.name,
                t.slots().count()
            )
        };
        self.open_confirm(Confirm::DeleteTrack(t.id), text);
    }

    fn set_bypass(&mut self, id: SlotId, bypassed: bool) {
        if let Some(s) = self.rig.get_mut(id) {
            s.bypassed = bypassed;
            self.engine.send(Command::SetBypass { slot: id, bypassed });
            self.refresh_slots();
        }
    }

    // =============================================================== editor

    fn select(&mut self, slot: Option<SlotId>) {
        // Tapping a crashed or failed plugin reloads it.
        if let Some(id) = slot {
            if matches!(
                self.runtime.get(&id).map(|r| &r.status),
                Some(SlotStatus::Crashed(_) | SlotStatus::Failed(_))
            ) {
                if let Some(s) = self.rig.get(id) {
                    let load = load_of(s);
                    self.runtime.entry(id).or_default().status = SlotStatus::Loading;
                    self.engine.send(Command::ReloadPlugin { load });
                }
            }
        }
        if let Some((id, t)) = slot.and_then(|id| Some((id, self.rig.track_of(id)?))) {
            self.view_track = t;
            self.track_selection.insert(t, id);
        }
        if self.selected != slot {
            self.hide_editor();
            self.selected = slot;
            if let Some(id) = slot {
                self.show_editor(id);
            }
        }
        if self.learning.is_some() {
            self.learning = None;
        }
        self.refresh_slots();
        self.refresh_midi_options();
        self.update_editor_ui();
    }

    fn show_editor(&mut self, slot: SlotId) {
        let Some(s) = self.rig.get(slot) else {
            return;
        };
        let name = s.name.clone();
        let rt = self.runtime.get(&slot);
        let info = rt.and_then(|r| r.info.clone());
        match (rt.map(|r| &r.status), info) {
            (Some(SlotStatus::Ready), Some(info)) if info.has_custom_ui => {
                self.engine.send(Command::ShowEditor { slot, show: true });
                let parent = if self.embed_available {
                    self.main_x11_window()
                } else {
                    None
                };
                if let (Some(parent), Some(h)) = (parent, &self.editor_host) {
                    h.send(HostCommand::Attach {
                        key: slot.0,
                        title: editor_window_title(slot),
                        parent,
                    });
                    self.editor = Some(EditorView { slot });
                    self.place_editor();
                    let ui = self.ui();
                    ui.set_editor_title(ss(&name));
                    ui.set_editor_mode(ss("waiting"));
                } else {
                    self.set_editor_message(&name, "The editor opened in its own window.");
                }
            }
            (Some(SlotStatus::Ready), Some(_)) => {
                self.editor = None;
                self.engine.send(Command::QueryParameters { slot });
                let ui = self.ui();
                ui.set_params(ModelRc::new(VecModel::<ParamItem>::default()));
                ui.set_editor_title(ss(&name));
                ui.set_editor_mode(ss("params"));
            }
            (Some(SlotStatus::Loading) | None, _) => {
                let ui = self.ui();
                ui.set_editor_title(ss(&name));
                ui.set_editor_mode(ss("waiting"));
            }
            _ => self.set_editor_message(
                &name,
                "This plugin is not running. Tap it in the chain to reload it.",
            ),
        }
    }

    fn hide_editor(&mut self) {
        if let Some(slot) = self.selected {
            self.engine.send(Command::ShowEditor { slot, show: false });
            if let Some(h) = &self.editor_host {
                h.send(HostCommand::Detach { key: slot.0 });
            }
        }
        self.editor = None;
        self.ui().set_editor_mode(ss("empty"));
    }

    fn set_editor_message(&self, title: &str, message: &str) {
        let ui = self.ui();
        ui.set_editor_title(ss(title));
        ui.set_editor_message(ss(message));
        ui.set_editor_mode(ss("message"));
    }

    fn update_editor_ui(&self) {
        let ui = self.ui();
        let o = ui.global::<Options>();
        match self.selected.and_then(|s| self.rig.get(s)) {
            Some(s) => o.set_selected_plugin(ss(&s.name)),
            None => o.set_selected_plugin(ss("")),
        }
    }

    /// The main window's X11 id, once it exists (None under Wayland).
    fn main_x11_window(&self) -> Option<u32> {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let mut id = None;
        self.with_winit(|w| {
            id = match w.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Xlib(h)) => Some(h.window as u32),
                Ok(RawWindowHandle::Xcb(h)) => Some(h.window.get()),
                _ => None,
            };
        });
        id
    }

    /// Tells the editor host where the main area is, and whether the editor
    /// may show: an X11 child window always covers what Slint draws, so the
    /// editor hides while a sidebar, the browser or a dialog is over it.
    fn place_editor(&mut self) {
        let Some(h) = &self.editor_host else { return };
        let ui = self.ui();
        let scale = ui.window().scale_factor();
        let px = |v: f32| (v * scale).round();
        let area = Area {
            x: px(ui.get_area_x()) as i32,
            y: px(ui.get_area_y()) as i32,
            width: px(ui.get_area_width()).max(0.0) as u32,
            height: px(ui.get_area_height()).max(0.0) as u32,
        };
        let state = (area, !ui.get_editor_covered());
        if self.editor_area != Some(state) {
            self.editor_area = Some(state);
            h.send(HostCommand::SetArea {
                area: state.0,
                visible: state.1,
            });
        }
    }

    fn on_host_event(&mut self, e: HostEvent) {
        match e {
            HostEvent::Attached { key, .. } => {
                if self.editor.as_ref().is_some_and(|ed| ed.slot.0 == key) {
                    self.ui().set_editor_mode(ss("embedded"));
                }
            }
            HostEvent::NotFound { key } => {
                if self.editor.as_ref().is_some_and(|ed| ed.slot.0 == key) {
                    let slot = SlotId(key);
                    let name = self
                        .rig
                        .get(slot)
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    log::warn!("editor window for {name} not found; showing generic controls");
                    self.editor = None;
                    self.engine.send(Command::QueryParameters { slot });
                    let ui = self.ui();
                    ui.set_editor_title(ss(&name));
                    ui.set_editor_mode(ss("params"));
                }
            }
            HostEvent::Lost { key } => {
                if self.editor.as_ref().is_some_and(|ed| ed.slot.0 == key) {
                    let name = self
                        .rig
                        .get(SlotId(key))
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.editor = None;
                    self.set_editor_message(
                        &format!("{name}: editor closed"),
                        "Tap the plugin in the chain to open it again.",
                    );
                    // Allow re-selecting the same plugin to reopen it.
                    self.selected = None;
                    self.refresh_slots();
                }
            }
            HostEvent::Unavailable(reason) => {
                log::warn!("editor embedding unavailable: {reason}");
                self.embed_available = false;
            }
        }
    }

    // ============================================================== presets

    fn refresh_presets(&self) {
        let ui = self.ui();
        let o = ui.global::<Options>();
        o.set_presets(strings(self.presets.list()));
        o.set_current_preset(ss(self.settings.current_preset.clone().unwrap_or_default()));
        o.set_default_preset(ss(self.settings.default_preset.clone().unwrap_or_default()));
    }

    fn save_preset(&mut self, name: &str) {
        self.capture_states();
        let preset = Preset::new(name.trim(), self.rig.clone(), self.mappings.clone());
        match self.presets.save(&preset) {
            Ok(()) => {
                self.settings.current_preset = Some(preset.name.clone());
                self.save_settings();
                self.toast(format!("Saved \"{}\"", preset.name), false);
            }
            Err(e) => self.toast(format!("Could not save preset: {e}"), true),
        }
        self.refresh_presets();
        self.refresh_slots();
    }

    fn load_preset(&mut self, name: &str) {
        let preset = match self.presets.load(name) {
            Ok(p) => p,
            Err(e) => {
                self.toast(format!("Could not load preset: {e:#}"), true);
                return;
            }
        };
        self.hide_editor();
        self.selected = None;
        self.undo = None;
        self.ui().set_undo_text(ss(""));
        self.rig = preset.rig;
        self.view_track = self.rig.ensure_track();
        self.mappings = preset.midi_mappings;
        self.runtime = self
            .rig
            .slots()
            .map(|s| (s.id, SlotRuntime::default()))
            .collect();
        self.settings.current_preset = Some(preset.name.clone());
        self.save_settings();
        if self.engine_running {
            self.load_rig();
        } else {
            self.reload_chain_on_start = true;
        }
        self.refresh_slots();
        self.refresh_presets();
        self.refresh_midi_options();
        self.update_editor_ui();
        self.toast(format!("Loaded \"{}\"", preset.name), false);
    }

    // =========================================================== text input

    fn open_prompt(&mut self, kind: Prompt, title: &str, text: String, secret: bool) {
        self.prompt = Some(kind);
        let ui = self.ui();
        ui.set_prompt_title(ss(title));
        ui.set_prompt_text(ss(text));
        ui.set_prompt_secret(secret);
        ui.set_prompt_open(true);
    }

    fn prompt_done(&mut self, ok: bool, text: String) {
        let Some(kind) = self.prompt.take() else {
            return;
        };
        if !ok {
            return;
        }
        match kind {
            Prompt::SaveAs => {
                if text.trim().is_empty() {
                    self.toast("Please enter a name", true);
                } else {
                    self.save_preset(&text);
                }
            }
            Prompt::Rename(old) => match self.presets.rename(&old, &text) {
                Ok(()) => {
                    let new = text.trim().to_string();
                    if self.settings.current_preset.as_deref() == Some(old.as_str()) {
                        self.settings.current_preset = Some(new.clone());
                    }
                    if self.settings.default_preset.as_deref() == Some(old.as_str()) {
                        self.settings.default_preset = Some(new);
                    }
                    self.save_settings();
                    self.refresh_presets();
                    self.refresh_slots();
                }
                Err(e) => self.toast(format!("Could not rename: {e}"), true),
            },
            Prompt::WifiPassword(ssid) => self.system.send(SysCmd::ConnectWifi {
                ssid,
                password: Some(text),
            }),
            Prompt::RenameTrack(id) => {
                let name = text.trim();
                if let (Some(t), false) = (self.rig.track_mut(id), name.is_empty()) {
                    t.name = name.to_string();
                }
                self.refresh_slots();
            }
            Prompt::Search => {
                self.ui().set_browser_search(ss(&text));
                self.filter.text = text;
                self.refresh_browser();
            }
        }
    }

    fn open_confirm(&mut self, kind: Confirm, text: String) {
        self.confirm = Some(kind);
        self.ui().set_confirm_text(ss(text));
    }

    fn confirm_done(&mut self, ok: bool) {
        let Some(kind) = self.confirm.take() else {
            return;
        };
        if !ok {
            return;
        }
        match kind {
            Confirm::DeletePreset(name) => {
                if let Err(e) = self.presets.delete(&name) {
                    self.toast(format!("Could not delete: {e}"), true);
                }
                if self.settings.current_preset.as_deref() == Some(name.as_str()) {
                    self.settings.current_preset = None;
                }
                if self.settings.default_preset.as_deref() == Some(name.as_str()) {
                    self.settings.default_preset = None;
                }
                self.save_settings();
                self.refresh_presets();
                self.refresh_slots();
            }
            Confirm::DeleteTrack(id) => {
                let Some((index, track)) = self.rig.remove_track(id) else {
                    return;
                };
                for s in track.slots() {
                    if self.selected == Some(s.id) {
                        self.select(None);
                    }
                    self.runtime.remove(&s.id);
                    self.mappings.retain(|m| m.slot != s.id);
                    self.engine.send(Command::RemovePlugin { slot: s.id });
                }
                self.rig.ensure_track();
                self.view_track = self.rig.tracks[index.min(self.rig.tracks.len() - 1)].id;
                self.rig_changed();
                self.refresh_midi_options();
            }
            Confirm::Reboot => self.system.send(SysCmd::Reboot),
            Confirm::Shutdown => self.system.send(SysCmd::PowerOff),
            Confirm::Restart => {
                self.restart_requested = true;
                self.quit();
            }
            Confirm::Quit => self.quit(),
        }
    }

    // ============================================================== browser

    fn refresh_browser(&self) {
        let items: Vec<PluginItem> = self
            .catalog
            .filtered(&self.filter)
            .into_iter()
            .take(600)
            .map(|p| PluginItem {
                key: ss(p.key()),
                name: ss(&p.name),
                maker: ss(&p.maker),
                format: ss(p.format.label()),
                category: ss(p.category.label()),
                favorite: self.catalog.is_favorite(p),
                bridged: p.yabridge,
                synth: p.is_synth,
            })
            .collect();
        let ui = self.ui();
        ui.set_browser_plugins(ModelRc::new(VecModel::from(items)));
        ui.set_browser_format(ss(self.filter.format.map(|f| f.label()).unwrap_or("")));
        ui.set_browser_category(ss(self.filter.category.map(|c| c.label()).unwrap_or("")));
        ui.set_browser_favorites(self.filter.favorites_only);
        ui.global::<Options>().set_plugin_count(ss(format!(
            "{} plugins in the library",
            self.catalog.plugins.len()
        )));
    }

    fn start_scan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        let tool = EngineHandle::discovery_tool();
        if !tool.exists() {
            self.toast(
                format!("Plugin scanner not found at {}", tool.display()),
                true,
            );
            return;
        }
        self.scan = Some(scan::start(tool));
        let ui = self.ui();
        ui.set_scanning(true);
        ui.set_scan_status(ss("Scanning for plugins…"));
    }

    fn on_scan(&mut self, m: ScanMsg) {
        match m {
            ScanMsg::Progress(p) => {
                let name = std::path::Path::new(&p.current)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or(p.current);
                self.ui()
                    .set_scan_status(ss(format!("Scanning {}/{}  {name}", p.done, p.total)));
            }
            ScanMsg::Done(report) => {
                self.scan = None;
                let found = report.plugins.len();
                self.catalog.replace_plugins(report.plugins);
                self.save_catalog();
                let ui = self.ui();
                ui.set_scanning(false);
                let failed = if report.failures.is_empty() {
                    String::new()
                } else {
                    format!(", {} could not be loaded", report.failures.len())
                };
                for (path, err) in &report.failures {
                    log::info!("scan: {path}: {err}");
                }
                ui.set_scan_status(ss(format!("Found {found} plugins{failed}")));
                self.refresh_browser();
            }
        }
    }

    fn save_catalog(&self) {
        if let Err(e) = self.catalog.save(&self.paths.plugin_cache_file()) {
            log::warn!("could not save plugin cache: {e}");
        }
    }

    // ================================================================= midi

    fn refresh_midi_options(&self) {
        let ui = self.ui();
        let o = ui.global::<Options>();
        let mut ports = vec!["All inputs".to_string()];
        ports.extend(MidiListener::ports());
        o.set_midi_ports(strings(ports));
        o.set_midi_port(ss(if self.settings.midi.learn_input.is_empty() {
            "All inputs"
        } else {
            &self.settings.midi.learn_input
        }));
        o.set_learning(self.learning.is_some());
        o.set_learn_target(ss(self
            .learning
            .and_then(|(s, _)| self.rig.get(s))
            .map(|s| s.name.clone())
            .unwrap_or_default()));
        let items: Vec<MappingItem> = self
            .mappings
            .iter()
            .enumerate()
            .map(|(i, m)| MappingItem {
                index: i as i32,
                trigger: ss(m.trigger.label()),
                plugin: ss(self
                    .rig
                    .get(m.slot)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| "(removed)".into())),
                action: ss(match m.action {
                    MidiAction::Toggle => "Toggles bypass on each press",
                    MidiAction::Follow => "Enabled while held",
                }),
            })
            .collect();
        o.set_mappings(ModelRc::new(VecModel::from(items)));
    }

    fn on_midi(&mut self, msg: MidiMessage) {
        if let Some((slot, action)) = self.learning {
            let Some((trigger, pressed)) = msg.trigger() else {
                return;
            };
            if !pressed {
                return;
            }
            self.mappings
                .retain(|m| !(m.trigger == trigger && m.slot == slot));
            self.mappings.push(MidiMapping {
                trigger,
                slot,
                action,
            });
            self.learning = None;
            let name = self
                .rig
                .get(slot)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            self.toast(format!("{} now controls {name}", trigger.label()), false);
            self.refresh_midi_options();
            self.refresh_slots();
            return;
        }
        for change in midi::resolve(&self.mappings, &msg) {
            match change {
                BypassChange::Toggle(slot) => {
                    if let Some(b) = self.rig.get(slot).map(|s| !s.bypassed) {
                        self.set_bypass(slot, b);
                    }
                }
                BypassChange::Set { slot, bypassed } => self.set_bypass(slot, bypassed),
            }
        }
    }

    // =============================================================== system

    fn connect_wifi(&mut self, ssid: String) {
        let ui = self.ui();
        let nets = ui.global::<Options>().get_wifi_networks();
        let net = (0..nets.row_count())
            .filter_map(|i| nets.row_data(i))
            .find(|n| n.ssid == ssid);
        match net {
            Some(n) if n.active => {}
            Some(n) if n.secured && !n.known => self.open_prompt(
                Prompt::WifiPassword(ssid.clone()),
                &format!("Password for {ssid}"),
                String::new(),
                true,
            ),
            _ => self.system.send(SysCmd::ConnectWifi {
                ssid,
                password: None,
            }),
        }
    }

    fn on_system_event(&mut self, e: SysEvent) {
        let ui = self.ui();
        let o = ui.global::<Options>();
        match e {
            SysEvent::Wifi {
                available,
                enabled,
                networks,
                status,
            } => {
                o.set_wifi_available(available);
                o.set_wifi_enabled(enabled);
                o.set_wifi_status(ss(status));
                let items: Vec<WifiItem> = networks
                    .into_iter()
                    .map(|n| WifiItem {
                        ssid: ss(&n.ssid),
                        strength: n.strength as i32,
                        secured: n.secured,
                        active: n.active,
                        known: n.known,
                    })
                    .collect();
                o.set_wifi_networks(ModelRc::new(VecModel::from(items)));
            }
            SysEvent::Bt {
                available,
                enabled,
                devices,
                scanning,
                status,
            } => {
                o.set_bt_available(available);
                o.set_bt_enabled(enabled);
                o.set_bt_scanning(scanning);
                o.set_bt_status(ss(status));
                let items: Vec<BtItem> = devices
                    .into_iter()
                    .map(|d| BtItem {
                        path: ss(&d.path),
                        name: ss(&d.name),
                        paired: d.paired,
                        connected: d.connected,
                        icon: ss(&d.icon),
                    })
                    .collect();
                o.set_bt_devices(ModelRc::new(VecModel::from(items)));
            }
            SysEvent::Brightness(p) => {
                o.set_brightness_available(p.is_some());
                if let Some(p) = p {
                    o.set_brightness(p as f32);
                }
            }
            SysEvent::Info(msg) => self.toast(msg, false),
            SysEvent::Error(msg) => self.toast(msg, true),
        }
    }

    // ============================================================= settings

    fn refresh_audio_options(&self) {
        let ui = self.ui();
        let o = ui.global::<Options>();
        let a = &self.settings.audio;
        o.set_driver(ss(&a.driver));
        o.set_device(ss(&a.device));
        o.set_input(ss(if a.input_client.is_empty() {
            "system"
        } else {
            &a.input_client
        }));
        o.set_output(ss(if a.output_client.is_empty() {
            "system"
        } else {
            &a.output_client
        }));
        o.set_sample_rate(ss(a.sample_rate.to_string()));
        o.set_buffer_size(ss(a.buffer_size.to_string()));
        o.set_isolate_plugins(a.isolate_plugins);
    }

    fn set_fullscreen(&mut self, fs: bool) {
        let ui = self.ui();
        if !fs {
            ui.window().set_fullscreen(false);
            let d = &self.settings.display;
            ui.window().set_size(slint::LogicalSize::new(
                d.window_width as f32,
                d.window_height as f32,
            ));
        } else {
            self.remember_window_size();
            ui.window().set_fullscreen(true);
        }
        self.settings.display.fullscreen = fs;
        ui.global::<Options>().set_fullscreen(fs);
        self.save_settings();
    }

    fn remember_window_size(&mut self) {
        let ui = self.ui();
        if self.settings.display.fullscreen
            || ui.window().is_fullscreen()
            || ui.window().is_maximized()
        {
            return;
        }
        let size = ui.window().size().to_logical(ui.window().scale_factor());
        if size.width >= 400.0 && size.height >= 300.0 {
            self.settings.display.window_width = size.width as u32;
            self.settings.display.window_height = size.height as u32;
        }
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.save(&self.paths.settings_file()) {
            log::warn!("could not save settings: {e}");
        }
    }

    /// Leaves the event loop.
    fn quit(&mut self) {
        self.release_editors();
        let _ = slint::quit_event_loop();
    }

    /// Hands the editor windows back to their plugins. They live inside the
    /// main window, and would be destroyed with it when the app closes,
    /// which kills the plugin (or its bridge process) with an X error.
    fn release_editors(&mut self) {
        self.hide_editor();
        // Dropping the host waits until every window has been handed back.
        self.editor_host = None;
        self.embed_available = false;
    }

    pub fn shutdown(&mut self) {
        self.remember_window_size();
        self.save_settings();
        self.release_editors();
        self.engine.send(Command::Stop);
        if self.restart_requested {
            let exe = std::env::var_os("APPIMAGE")
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::current_exe().ok());
            if let Some(exe) = exe {
                // Give the engine a moment to release the audio device.
                std::thread::sleep(Duration::from_millis(300));
                if let Err(e) = std::process::Command::new(exe)
                    .args(std::env::args().skip(1))
                    .spawn()
                {
                    log::error!("could not restart: {e}");
                }
            }
        }
    }
}
