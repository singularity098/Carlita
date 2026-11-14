//! Application state and the glue between the UI, the engine, the editor
//! host and the system workers.

use crate::midi_in::MidiListener;
use crate::scan::{self, ScanMsg};
use crate::system::{SysCmd, SysEvent, SystemWorker};
use crate::{
    BtItem, IoChoice, MainWindow, MappingItem, Options, ParamItem, PluginItem, Recording, SlotItem,
    TextUtil, Theme, TrackIo, TrackItem, WifiItem,
};
use carlita_core::midi::{self, BypassChange, MidiAction, MidiMapping, MidiMessage};
use carlita_core::paths::AppPaths;
use carlita_core::rig::{self, Location, Row, RowKind, Spot};
use carlita_core::settings::{DisplaySettings, RecordFormat};
use carlita_core::{
    CatalogFilter, ChainSlot, PluginCatalog, PluginCategory, PluginFormat, Preset, PresetStore,
    Rig, Settings, SlotId, TrackId, TrackMidi,
};
use carlita_engine::{
    editor_window_title, Command, EngineConfig, EngineHandle, Event, Meters, MidiDevice,
    PluginLoad, PluginRuntimeInfo,
};
use carlita_x11::{Area, EditorHost, HostCommand, HostEvent};
use crossbeam_channel::Receiver;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::meters::MeterBars;
use crate::recorder::{safe_name, Recorder, TrackSpec};
use std::path::{Path, PathBuf};

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
    NewProject,
    ProjectsFolder,
}

enum Confirm {
    DeletePreset(String),
    DeleteTrack(TrackId),
    Reboot,
    Shutdown,
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
    /// When the first start was asked for: it waits for the main window to
    /// exist, so plugin editors get the scale it is drawn at.
    start_pending: Option<Instant>,
    /// The scale the main window is drawn at.
    ui_scale: f32,
    first_start: bool,
    skip_default_preset: bool,
    reload_chain_on_start: bool,
    tried_fallback: bool,
    editor_host: Option<EditorHost>,
    /// Whether editors can be shown inside the main window (X11 only).
    embed_available: bool,
    /// What the editor host was last told: the main area, and the part of
    /// it nothing covers.
    editor_area: Option<(Area, Area)>,
    /// The edge strips last sent to the editor host, with the main window.
    edge_state: Option<(u32, Vec<Area>)>,
    system: SystemWorker,
    midi: MidiListener,
    learning: Option<(SlotId, MidiAction)>,
    scan: Option<Receiver<ScanMsg>>,
    prompt: Option<Prompt>,
    confirm: Option<Confirm>,
    toast_at: Option<Instant>,
    options_page: i32,
    last_sys_refresh: Instant,
    /// The chain sidebar was out at the last poll (its recording card lists
    /// the projects, read again each time it comes out).
    chain_open: bool,
    slots_model: Rc<VecModel<SlotItem>>,
    tracks_model: Rc<VecModel<TrackItem>>,
    meters: Meters,
    /// The chain panel's meters as drawn: input, output and track, left
    /// and right of each.
    bars: MeterBars<6>,
    /// The DSP load and latency texts last shown, and when.
    meter_texts: ((String, String), Instant),
    /// Port names of the hardware inputs tracks can take, in order.
    hw_inputs: Vec<String>,
    /// MIDI devices tracks can take.
    midi_devices: Vec<MidiDevice>,
    /// Every track takes every MIDI device (not under JACK).
    merged_midi: bool,
    /// The track's input page is open, so the hardware inputs are metered
    /// one by one to show which have a signal.
    probing: bool,
    /// The probe count last sent to the engine.
    probe_sent: Option<u16>,
    /// Until when each hardware input shows a signal.
    signal_until: Vec<Instant>,
    /// Whether each hardware input shows a signal.
    signal_model: Rc<VecModel<bool>>,
    /// The timer that runs `poll`, paced to the display's refresh rate.
    poll_timer: Rc<slint::Timer>,
    /// The driver the engine runs on ("" while it is stopped).
    engine_driver: String,
    /// The recording project's folder, while one is open.
    project: Option<PathBuf>,
    /// The stem recorder's JACK client, while a project is open and the
    /// engine runs on JACK.
    recorder: Option<Recorder>,
    /// The take's length last shown, and the gaps last reported.
    rec_time: String,
    rec_dropped: u64,
    /// When the window's size and place were last noted.
    last_window_check: Instant,
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
            start_pending: None,
            ui_scale: 1.0,
            first_start: true,
            skip_default_preset: opts.skip_default_preset,
            reload_chain_on_start: false,
            tried_fallback: false,
            embed_available: editor_host.is_some(),
            editor_area: None,
            edge_state: None,
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
            chain_open: false,
            slots_model,
            tracks_model,
            meters: Meters::default(),
            bars: MeterBars::new(),
            meter_texts: (Default::default(), Instant::now()),
            hw_inputs: Vec::new(),
            midi_devices: Vec::new(),
            merged_midi: false,
            probing: false,
            probe_sent: None,
            signal_until: Vec::new(),
            signal_model: Rc::new(VecModel::default()),
            poll_timer: Rc::new(slint::Timer::default()),
            engine_driver: String::new(),
            project: None,
            recorder: None,
            rec_time: String::new(),
            rec_dropped: 0,
            last_window_check: Instant::now(),
        }));

        Self::wire(ui, &app);
        {
            let mut a = app.borrow_mut();
            a.init_ui(ui);
            a.engine.send(Command::QueryDrivers);
            a.start_pending = Some(Instant::now());
            if a.catalog.plugins.is_empty() {
                a.start_scan();
            }
            a.system.send(SysCmd::RefreshWifi);
            a.system.send(SysCmd::RefreshBt);
        }

        // Poll engine/editor/system events at display rate (60 Hz until the
        // window is up and the display's own rate is known).
        let timer = app.borrow().poll_timer.clone();
        let poll_app = Rc::downgrade(&app);
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                if let Some(app) = poll_app.upgrade() {
                    if let Ok(mut a) = app.try_borrow_mut() {
                        a.poll();
                    }
                }
            },
        );
        Ok(app)
    }

    fn ui(&self) -> MainWindow {
        self.ui.upgrade().expect("main window alive")
    }

    fn init_ui(&mut self, ui: &MainWindow) {
        let d = &self.settings.display;
        // Where the window was, also when fullscreen: that picks the
        // monitor it fills.
        if let (Some(x), Some(y)) = (d.window_x, d.window_y) {
            ui.window().set_position(slint::PhysicalPosition::new(x, y));
        }
        if !d.fullscreen {
            ui.window().set_size(slint::LogicalSize::new(
                d.window_width as f32,
                d.window_height as f32,
            ));
            if d.maximized {
                ui.window().set_maximized(true);
                ui.set_window_maximized(true);
            }
        }
        ui.window().set_fullscreen(d.fullscreen);
        ui.set_strip_width(d.edge_swipe_width as f32);
        apply_theme(ui, &d.theme);
        ui.set_browser_formats(strings(PluginFormat::ALL.iter().map(|f| f.label())));
        ui.set_browser_categories(strings(PluginCategory::ALL.iter().map(|c| c.label())));
        ui.set_keyboard_visible(self.settings.show_keyboard);
        ui.set_keyboard_base(48 + 12 * self.settings.midi.keyboard_octave as i32);

        let o = ui.global::<Options>();
        o.set_version(ss(env!("CARGO_PKG_VERSION")));
        o.set_fullscreen(d.fullscreen);
        o.set_editor_zooms(strings(
            DisplaySettings::EDITOR_ZOOMS
                .iter()
                .map(|z| format!("{z}%")),
        ));
        o.set_editor_zoom(ss(format!("{}%", d.editor_zoom)));
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
        tu.on_edit(|text, anchor, cursor, insert, backspace| {
            ss(edit_text(&text, anchor, cursor, &insert, backspace).0)
        });
        tu.on_edit_cursor(|text, anchor, cursor, insert, backspace| {
            edit_text(&text, anchor, cursor, &insert, backspace).1 as i32
        });
        tu.on_port_label(|s| ss(port_label(&s)));

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
            if a.settings.display.fullscreen {
                // From the fullscreen track bar: leave fullscreen for a
                // maximized window.
                a.settings.display.maximized = true;
                a.set_fullscreen(false);
                return;
            }
            let m = !ui.window().is_maximized();
            if m {
                a.remember_window_size();
            }
            ui.window().set_maximized(m);
            ui.set_window_maximized(m);
            a.settings.display.maximized = m;
            a.save_settings();
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
            let id = a.rig.add_track(None);
            a.rig_changed();
            // Show the new (empty) track: the previous track's plugin editor
            // goes away rather than look as if it were on the new track.
            a.select_track(id);
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
        on!(ui, on_rename_track_to, |a, id: i32, name: SharedString| {
            let name = name.trim();
            if let (Some(t), false) = (a.rig.track_mut(TrackId(id as u64)), name.is_empty()) {
                t.name = name.to_string();
            }
            a.refresh_slots();
        });
        on!(ui, on_delete_track, |a| a.delete_track(a.view_track));
        on!(ui, on_close_track, |a, id: i32| a
            .delete_track(TrackId(id as u64)));
        let io = ui.global::<TrackIo>();
        on!(io, on_toggle_input, |a, i: i32| {
            if let Some(t) = a.rig.track_mut(a.view_track) {
                t.input.toggle(i.clamp(0, u16::MAX as i32) as u16);
                a.rig_changed();
            }
        });
        on!(io, on_probe, |a, on: bool| {
            a.probing = on;
            a.update_probe();
        });
        on!(io, on_toggle_midi, |a, i: i32| {
            let ids = a.midi_ids();
            let Some(id) = a
                .midi_choices()
                .get(i as usize)
                .map(|(id, _, _)| id.clone())
            else {
                return;
            };
            if let Some(t) = a.rig.track_mut(a.view_track) {
                t.midi.toggle(&id, &ids);
                a.rig_changed();
            }
        });
        on!(io, on_set_midi_all, |a, all: bool| {
            if let Some(t) = a.rig.track_mut(a.view_track) {
                // Turning "all devices" off turns every device off, as
                // turning it on turns them all on.
                t.midi = if all {
                    TrackMidi::All
                } else {
                    TrackMidi::Devices(Vec::new())
                };
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
                6 => a.refresh_recording(),
                _ => {}
            }
        });

        // ---- recording
        let r = ui.global::<Recording>();
        on!(r, on_new_project, |a| a.open_prompt(
            Prompt::NewProject,
            "Name of the new project",
            String::new(),
            false
        ));
        on!(r, on_open_project, |a, name: SharedString| {
            let path = a.projects_dir().join(name.as_str());
            a.open_project(path);
        });
        on!(r, on_close_project, |a| a.close_project());
        on!(r, on_change_location, |a| {
            let text = a.projects_dir().display().to_string();
            a.open_prompt(Prompt::ProjectsFolder, "Projects folder", text, false);
        });
        on!(r, on_set_dry, |a, v: bool| {
            a.settings.recording.record_dry = v;
            a.save_settings();
            a.sync_recorder();
            a.refresh_recording();
        });
        on!(r, on_set_format, |a, v: i32| {
            a.settings.recording.format = if v == 1 {
                RecordFormat::Flac
            } else {
                RecordFormat::Wav
            };
            a.save_settings();
            a.refresh_recording();
        });
        on!(r, on_record, |a| a.record());
        on!(r, on_pause, |a| a.pause_recording());
        on!(r, on_stop, |a| a.stop_recording());

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
        on!(o, on_set_input_count, |a, v: SharedString| {
            a.settings.audio.input_count = v.parse().unwrap_or(0);
            a.save_settings();
            a.refresh_audio_options();
            a.refresh_track_io();
            a.update_probe();
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
        on!(o, on_set_editor_zoom, |a, v: SharedString| {
            let Ok(zoom) = v.trim_end_matches('%').trim().parse::<u32>() else {
                return;
            };
            if zoom == a.settings.display.editor_zoom {
                return;
            }
            a.settings.display.editor_zoom = zoom;
            a.ui()
                .global::<Options>()
                .set_editor_zoom(ss(format!("{zoom}%")));
            // Plugins read their scale when they start (bridges from their
            // environment), so they are started again.
            if a.engine_running {
                a.restart_engine();
                a.toast(
                    format!("Plugin editors at {zoom}%: restarting the audio engine"),
                    false,
                );
            } else {
                a.save_settings();
            }
        });
        on!(o, on_set_brightness, |a, v: f32| {
            a.settings.display.brightness = Some(v.round() as u32);
            a.system.send(SysCmd::SetBrightness(v.round() as u32));
            a.ui().global::<Options>().set_brightness(v);
            a.save_settings();
        });

        // ---- options: system
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
        if let Some(since) = self.start_pending {
            let mut scale = None;
            self.with_winit(|w| scale = Some(w.scale_factor()));
            // Give up waiting after a while (no window yet: start anyway).
            if scale.is_some() || since.elapsed() > Duration::from_secs(3) {
                self.start_pending = None;
                self.ui_scale = scale.unwrap_or(1.0) as f32;
                log::info!("drawing at scale {}", self.ui_scale);
                self.keep_window_on_screen();
                self.pace_to_display();
                self.start_engine();
            }
        }
        while let Ok(e) = self.engine.events().try_recv() {
            self.on_engine_event(e);
        }
        self.animate_meters();
        self.show_take_time();
        let host_events: Vec<HostEvent> = self
            .editor_host
            .as_ref()
            .map(|h| h.events().try_iter().collect())
            .unwrap_or_default();
        for e in host_events {
            self.on_host_event(e);
        }
        // Also from here, not only from `area-changed`: a sidebar or the
        // track bar sliding in by itself may not report every step.
        if self.editor.is_some() {
            self.place_editor();
        }
        self.place_edges();
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
        if self.last_window_check.elapsed() > Duration::from_millis(500) {
            self.last_window_check = Instant::now();
            self.note_window_state();
        }
        if self.toast_at.is_some_and(|t| t.elapsed() > TOAST_TIMEOUT) {
            self.toast_at = None;
            self.ui().set_toast(ss(""));
        }
        let chain_open = self.ui().get_left_pos() > 0.0;
        if chain_open && !self.chain_open {
            self.refresh_recording();
        }
        self.chain_open = chain_open;
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
            // The desktop's scale, times the editor size picked in Display
            // options.
            ui_scale: self.ui_scale * self.settings.display.editor_zoom_factor(),
        }
    }

    fn start_engine(&mut self) {
        self.start_pending = None;
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

    fn load_rig(&mut self) {
        self.engine.send(Command::LoadRig {
            loads: self.loads(),
            layout: self.rig.layout(),
        });
        self.sync_recorder();
    }

    /// Sends the rig's structure to the engine and redraws the chain.
    fn rig_changed(&mut self) {
        self.engine.send(Command::SetLayout(self.rig.layout()));
        self.sync_recorder();
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
                self.engine_driver = driver.clone();
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
                        _ => self.engine.send(Command::SetLayout(self.rig.layout())),
                    }
                } else if self.reload_chain_on_start {
                    self.reload_chain_on_start = false;
                    self.load_rig();
                } else {
                    self.engine.send(Command::SetLayout(self.rig.layout()));
                }
                self.engine.send(Command::QueryDrivers);
                self.probe_sent = None;
                self.update_probe();
                self.sync_recorder();
                self.refresh_recording();
            }
            Event::StartFailed(err) => {
                self.engine_running = false;
                self.engine_driver.clear();
                self.drop_recorder();
                self.refresh_recording();
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
                self.engine_driver.clear();
                self.drop_recorder();
                self.refresh_recording();
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
            Event::Hardware {
                inputs,
                midi,
                merged_midi,
            } => {
                self.hw_inputs = inputs;
                self.midi_devices = midi;
                self.merged_midi = merged_midi;
                self.refresh_track_io();
                self.refresh_audio_options();
                self.update_probe();
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
                            default: p.default,
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
        // The chain panel shows the selected track's inputs.
        let input = m
            .track_inputs
            .iter()
            .find(|(t, _)| *t == self.view_track)
            .map_or(m.input, |(_, l)| *l);
        let track = m
            .tracks
            .iter()
            .find(|(t, _)| *t == self.view_track)
            .map_or([0.0; 2], |(_, l)| *l);
        // Drawn by `animate_meters`, once per display frame.
        self.bars.set([
            input[0],
            input[1],
            m.output[0],
            m.output[1],
            track[0],
            track[1],
        ]);
        self.show_signals(&m.hardware_inputs);
        let load = format!("{:.0}%", m.dsp_load);
        let latency = format!("{:.1} ms", m.total_latency_ms());
        // Only when they change, and a few times a second at most: the load
        // changes with nearly every report, and each change redraws the
        // whole window.
        if self.meter_texts.0 != (load.clone(), latency.clone())
            && self.meter_texts.1.elapsed() >= Duration::from_millis(250)
        {
            ui.set_dsp_load(ss(&load));
            ui.set_latency(ss(&latency));
            let o = ui.global::<Options>();
            o.set_dsp_load(ss(&load));
            o.set_latency(ss(&latency));
            self.meter_texts = ((load.clone(), latency.clone()), Instant::now());
        }
        if m.xruns != self.meters.xruns {
            ui.global::<Options>().set_xruns(m.xruns as i32);
        }
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

    /// Draws the chain panel's meters, falling smoothly between the
    /// engine's reports. Called once per display frame.
    fn animate_meters(&mut self) {
        let Some(l) = self.bars.step(Instant::now()) else {
            return;
        };
        let ui = self.ui();
        ui.set_in_left(l[0]);
        ui.set_in_right(l[1]);
        ui.set_out_left(l[2]);
        ui.set_out_right(l[3]);
        ui.set_track_left(l[4]);
        ui.set_track_right(l[5]);
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
            ui.set_track_muted(t.muted);
            ui.set_track_volume(t.volume);
            ui.set_track_volume_text(ss(volume_text(t.volume)));
        }
        ui.set_preset_name(ss(self.settings.current_preset.clone().unwrap_or_default()));
        self.refresh_track_io();
    }

    /// How many hardware inputs tracks are offered: the ports there are,
    /// or fewer when the options say the interface has fewer.
    fn input_count(&self) -> usize {
        match self.settings.audio.input_count as usize {
            0 => self.hw_inputs.len(),
            n => n.min(self.hw_inputs.len()),
        }
        .min(rig::MAX_INPUTS)
    }

    /// Meters the offered hardware inputs while the input page is open.
    fn update_probe(&mut self) {
        let count = if self.probing && self.engine_running {
            self.input_count() as u16
        } else {
            0
        };
        if self.probe_sent != Some(count) {
            self.probe_sent = Some(count);
            self.engine.send(Command::ProbeInputs { count });
        }
        if count == 0 {
            self.show_signals(&[]);
        }
    }

    /// Lights the hardware inputs that hear something, and keeps each lit
    /// a moment after it goes quiet, so it does not flicker.
    fn show_signals(&mut self, levels: &[f32]) {
        /// About -42 dBFS: above the hiss of an idle input, below the
        /// quietest playing.
        const THRESHOLD: f32 = 0.008;
        const HOLD: Duration = Duration::from_millis(300);
        let now = Instant::now();
        self.signal_until.resize(levels.len(), now);
        for (until, level) in self.signal_until.iter_mut().zip(levels) {
            if *level >= THRESHOLD {
                *until = now + HOLD;
            }
        }
        let model = &self.signal_model;
        while model.row_count() > levels.len() {
            model.remove(model.row_count() - 1);
        }
        for (k, until) in self.signal_until.iter().enumerate() {
            let on = *until > now;
            match model.row_data(k) {
                Some(v) if v == on => {}
                Some(_) => model.set_row_data(k, on),
                None => model.push(on),
            }
        }
    }

    /// Ids of the MIDI devices there are now.
    fn midi_ids(&self) -> Vec<String> {
        self.midi_devices.iter().map(|d| d.id.clone()).collect()
    }

    /// The MIDI devices to list for the selected track as (id, name,
    /// detail): the devices there are, then devices the track takes that
    /// are not plugged in.
    fn midi_choices(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<(String, String, String)> = self
            .midi_devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone(), d.id.clone()))
            .collect();
        if let Some(TrackMidi::Devices(ids)) = self.rig.track(self.view_track).map(|t| &t.midi) {
            for id in ids {
                if !v.iter().any(|(x, _, _)| x == id) {
                    v.push((id.clone(), midi_name(id), "Not plugged in".into()));
                }
            }
        }
        v
    }

    /// Shows what feeds the selected track.
    fn refresh_track_io(&self) {
        let Some(t) = self.rig.track(self.view_track) else {
            return;
        };
        let ui = self.ui();
        let io = ui.global::<TrackIo>();
        // Channels the track takes that the interface does not have (a
        // preset made on a bigger one) are listed too.
        let count = self
            .input_count()
            .max(t.input.channels().last().map_or(0, |c| *c as usize + 1))
            .min(rig::MAX_INPUTS);
        let inputs: Vec<IoChoice> = (0..count)
            .map(|k| IoChoice {
                name: ss((k + 1).to_string()),
                detail: ss(self
                    .hw_inputs
                    .get(k)
                    .map_or("Not available on this audio device", |n| n.as_str())),
                on: t.input.contains(k as u16),
            })
            .collect();
        let midi: Vec<IoChoice> = self
            .midi_choices()
            .into_iter()
            .map(|(id, name, detail)| IoChoice {
                name: ss(name),
                detail: ss(detail),
                on: t.midi.takes(&id),
            })
            .collect();
        let midi_label = match &t.midi {
            _ if self.merged_midi => "All MIDI".to_string(),
            TrackMidi::All => "All MIDI".to_string(),
            TrackMidi::Devices(d) if d.is_empty() => "No MIDI".to_string(),
            TrackMidi::Devices(d) if d.len() == 1 => self
                .midi_devices
                .iter()
                .find(|x| x.id == d[0])
                .map_or_else(|| midi_name(&d[0]), |x| x.name.clone()),
            TrackMidi::Devices(d) => format!("{} MIDI", d.len()),
        };
        io.set_input_label(ss(t.input.label()));
        io.set_midi_label(ss(midi_label));
        io.set_inputs(ModelRc::new(VecModel::from(inputs)));
        io.set_midi(ModelRc::new(VecModel::from(midi)));
        io.set_midi_all(t.midi == TrackMidi::All);
        io.set_merged_midi(self.merged_midi);
        io.set_signal(ModelRc::from(self.signal_model.clone()));
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

    fn delete_track(&mut self, id: TrackId) {
        let Some(t) = self.rig.track(id) else {
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
                let parent = if self.embed_available {
                    self.main_x11_window()
                } else {
                    None
                };
                if let (Some(parent), Some(h)) = (parent, &self.editor_host) {
                    self.clear_editor_snapshot();
                    // The host notes the windows already shown before the
                    // editor is opened (see HostCommand::Attach).
                    h.send(HostCommand::Attach {
                        key: slot.0,
                        title: editor_window_title(slot),
                        parent,
                        resizable: info.resizable_ui,
                    });
                    self.engine.send(Command::ShowEditor { slot, show: true });
                    self.editor = Some(EditorView { slot });
                    self.place_editor();
                    let ui = self.ui();
                    ui.set_editor_title(ss(&name));
                    ui.set_editor_mode(ss("waiting"));
                } else {
                    self.engine.send(Command::ShowEditor { slot, show: true });
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
        self.clear_editor_snapshot();
        self.ui().set_editor_mode(ss("empty"));
    }

    fn clear_editor_snapshot(&self) {
        let ui = self.ui();
        ui.set_editor_snapshot(slint::Image::default());
        ui.set_snapshot_width(0.0);
        ui.set_snapshot_height(0.0);
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

    /// Tells the editor host where the main area is, and which part of it
    /// nothing covers: an X11 child window always covers what Slint draws,
    /// so the editor is cut down to the part a sidebar or the track bar
    /// leaves, and hides under the browser or a dialog.
    fn place_editor(&mut self) {
        let Some(h) = &self.editor_host else { return };
        let ui = self.ui();
        let scale = ui.window().scale_factor();
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            let (x0, y0) = ((x * scale).round(), (y * scale).round());
            let x1 = ((x + w.max(0.0)) * scale).round();
            let y1 = ((y + h.max(0.0)) * scale).round();
            Area {
                x: x0 as i32,
                y: y0 as i32,
                width: (x1 - x0).max(0.0) as u32,
                height: (y1 - y0).max(0.0) as u32,
            }
        };
        let area = rect(
            ui.get_area_x(),
            ui.get_area_y(),
            ui.get_area_width(),
            ui.get_area_height(),
        );
        let uncovered = rect(
            ui.get_uncovered_x(),
            ui.get_uncovered_y(),
            ui.get_uncovered_width(),
            ui.get_uncovered_height(),
        );
        let state = (area, uncovered);
        if self.editor_area != Some(state) {
            self.editor_area = Some(state);
            h.send(HostCommand::SetArea { area, uncovered });
        }
    }

    /// Tells the editor host where the edge strips are (see
    /// `HostCommand::SetEdges`), so they work over an editor too.
    fn place_edges(&mut self) {
        if !self.embed_available || self.editor_host.is_none() {
            return;
        }
        let Some(parent) = self
            .edge_state
            .as_ref()
            .map(|e| e.0)
            .or_else(|| self.main_x11_window())
        else {
            return;
        };
        let ui = self.ui();
        let scale = ui.window().scale_factor();
        let size = ui.window().size();
        let (w, h) = (size.width as f32 / scale, size.height as f32 / scale);
        // Inside the resize handles in a window with a frame (see
        // `edge-inset` in main.slint).
        let (inset, sw) = (ui.get_edge_inset(), ui.get_edge_width());
        let (top, bottom) = (ui.get_edge_top(), ui.get_edge_bottom());
        let rect = |x: f32, y: f32, rw: f32, rh: f32| {
            let (x0, y0) = ((x * scale).round(), (y * scale).round());
            let x1 = ((x + rw.max(0.0)) * scale).round();
            let y1 = ((y + rh.max(0.0)) * scale).round();
            Area {
                x: x0 as i32,
                y: y0 as i32,
                width: (x1 - x0).max(0.0) as u32,
                height: (y1 - y0).max(0.0) as u32,
            }
        };
        let none = Area::default();
        let side = inset + sw;
        let edges = vec![
            rect(inset, top, sw, bottom - top),
            rect(w - side, top, sw, bottom - top),
            if ui.get_edge_top_strip() {
                rect(side, inset, w - 2.0 * side, sw)
            } else {
                none
            },
            if ui.get_edge_bottom_strip() {
                rect(side, h - side, w - 2.0 * side, sw)
            } else {
                none
            },
        ];
        if self.edge_state.as_ref() != Some(&(parent, edges.clone())) {
            if let Some(host) = &self.editor_host {
                host.send(HostCommand::SetEdges {
                    parent,
                    edges: edges.clone(),
                });
            }
            self.edge_state = Some((parent, edges));
        }
    }

    fn on_host_event(&mut self, e: HostEvent) {
        match e {
            HostEvent::ToggleFullscreen => {
                let fs = !self.settings.display.fullscreen;
                self.set_fullscreen(fs);
            }
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
                    // Close the editor if it did open somewhere, rather than
                    // leave a window of its own behind.
                    self.engine.send(Command::ShowEditor { slot, show: false });
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
            HostEvent::Snapshot {
                key,
                x,
                y,
                width,
                height,
                rgba,
            } => {
                if self.editor.as_ref().is_some_and(|ed| ed.slot.0 == key) {
                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &rgba, width, height,
                    );
                    let ui = self.ui();
                    let scale = ui.window().scale_factor();
                    ui.set_editor_snapshot(slint::Image::from_rgba8(buf));
                    ui.set_snapshot_x(x as f32 / scale);
                    ui.set_snapshot_y(y as f32 / scale);
                    ui.set_snapshot_width(width as f32 / scale);
                    ui.set_snapshot_height(height as f32 / scale);
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
        let preset = Preset::new(name.trim(), self.rig.clone(), self.mappings.clone())
            .with_selection(self.selected, self.track_selection.values().copied());
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
        // The plugins each track showed when the preset was saved.
        self.track_selection = preset
            .track_selections
            .iter()
            .filter_map(|&s| Some((self.rig.track_of(s)?, s)))
            .collect();
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
        // Show the plugin that was shown when the preset was saved (its
        // editor opens once it has loaded), not the first one to load.
        if let Some(slot) = preset.selected.filter(|s| self.rig.get(*s).is_some()) {
            self.select(Some(slot));
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
            Prompt::NewProject => {
                if text.trim().is_empty() {
                    self.toast("Please enter a name", true);
                } else {
                    let path = self.projects_dir().join(safe_name(&text));
                    self.open_project(path);
                }
            }
            Prompt::ProjectsFolder => {
                let path = expand_home(text.trim());
                match std::fs::create_dir_all(&path) {
                    Ok(()) => {
                        self.settings.recording.projects_dir = path.display().to_string();
                        self.save_settings();
                        self.refresh_recording();
                    }
                    Err(e) => self.toast(format!("Could not use {}: {e}", path.display()), true),
                }
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
        }
    }

    // ============================================================== browser

    fn refresh_browser(&self) {
        let items: Vec<PluginItem> = self
            .catalog
            .filtered(&self.filter)
            .into_iter()
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
                let log_file = self.paths.scan_log_file();
                let wrote = std::fs::create_dir_all(&self.paths.cache_dir)
                    .and_then(|_| std::fs::write(&log_file, &report.log));
                let details = match wrote {
                    Ok(()) if !report.failures.is_empty() => {
                        format!(" (details in {})", log_file.display())
                    }
                    Ok(()) => String::new(),
                    Err(e) => {
                        log::warn!("could not write {}: {e}", log_file.display());
                        String::new()
                    }
                };
                ui.set_scan_status(ss(format!("Found {found} plugins{failed}{details}")));
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
        // "All", then every count up to the ports there are (and the one
        // picked, when the interface is not there now).
        let most = self
            .hw_inputs
            .len()
            .max(a.input_count as usize)
            .min(rig::MAX_INPUTS);
        let mut counts = vec!["All".to_string()];
        counts.extend((1..=most).map(|n| n.to_string()));
        o.set_input_counts(strings(counts));
        o.set_input_count(ss(match a.input_count {
            0 => "All".to_string(),
            n => n.to_string(),
        }));
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
            if let (Some(x), Some(y)) = (d.window_x, d.window_y) {
                ui.window().set_position(slint::PhysicalPosition::new(x, y));
            }
            if d.maximized {
                ui.window().set_maximized(true);
                ui.set_window_maximized(true);
            }
        } else {
            self.note_window_state();
            ui.window().set_fullscreen(true);
        }
        self.settings.display.fullscreen = fs;
        ui.global::<Options>().set_fullscreen(fs);
        self.save_settings();
    }

    /// Notes whether the window is maximized and, when it is a plain window,
    /// its size and place, so the next start opens it the same way.
    fn note_window_state(&mut self) {
        let ui = self.ui();
        let w = ui.window();
        if self.settings.display.fullscreen || w.is_fullscreen() || w.is_minimized() {
            return;
        }
        self.settings.display.maximized = w.is_maximized();
        self.remember_window_size();
    }

    fn remember_window_size(&mut self) {
        let ui = self.ui();
        let w = ui.window();
        if self.settings.display.fullscreen
            || w.is_fullscreen()
            || w.is_maximized()
            || w.is_minimized()
        {
            return;
        }
        let size = w.size().to_logical(w.scale_factor());
        if size.width >= 400.0 && size.height >= 300.0 {
            self.settings.display.window_width = size.width as u32;
            self.settings.display.window_height = size.height as u32;
        }
        let mut pos = None;
        self.with_winit(|win| pos = win.outer_position().ok());
        if let Some(p) = pos {
            self.settings.display.window_x = Some(p.x);
            self.settings.display.window_y = Some(p.y);
        }
    }

    /// Moves the window back onto a monitor when the place it was saved at
    /// is no longer on any (a monitor was unplugged or rearranged).
    fn keep_window_on_screen(&self) {
        self.with_winit(|w| {
            let Ok(pos) = w.outer_position() else { return };
            let size = w.outer_size();
            let (cx, cy) = (
                pos.x + size.width as i32 / 2,
                pos.y + size.height as i32 / 2,
            );
            let on_screen = w.available_monitors().any(|m| {
                let (mp, ms) = (m.position(), m.size());
                cx >= mp.x
                    && cy >= mp.y
                    && cx < mp.x + ms.width as i32
                    && cy < mp.y + ms.height as i32
            });
            if !on_screen {
                if let Some(m) = w
                    .primary_monitor()
                    .or_else(|| w.available_monitors().next())
                {
                    log::info!("the saved window place is off screen; moving the window back");
                    w.set_outer_position(m.position());
                }
            }
        });
    }

    /// Runs `poll` (meters, editor placement, ...) once per frame of the
    /// display the window is on, so a 120 Hz display gets 120 updates a second.
    fn pace_to_display(&self) {
        let mut mhz = None;
        self.with_winit(|w| {
            mhz = w
                .current_monitor()
                .and_then(|m| m.refresh_rate_millihertz())
        });
        if let Some(mhz) = mhz.filter(|&m| m >= 30_000) {
            let interval = Duration::from_micros(1_000_000_000 / mhz as u64);
            log::info!("display refresh {:.1} Hz", mhz as f64 / 1000.0);
            self.poll_timer.set_interval(interval);
            self.engine.send(Command::SetMeterInterval(interval));
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
        self.note_window_state();
        self.save_settings();
        // Finishes a take that is still being recorded.
        self.drop_recorder();
        self.release_editors();
        self.engine.send(Command::Stop);
    }
}

/// Types `insert` into `text` at a text field's selection (`anchor` to
/// `cursor`, byte offsets), replacing what is selected; with `backspace`,
/// deletes the selection, or the character before the cursor when nothing
/// is selected. Returns the new text and the cursor's new byte offset.
fn edit_text(
    text: &str,
    anchor: i32,
    cursor: i32,
    insert: &str,
    backspace: bool,
) -> (String, usize) {
    // Offsets from the field are on character boundaries; clamp anyway.
    let fix = |o: i32| {
        let mut o = (o.max(0) as usize).min(text.len());
        while !text.is_char_boundary(o) {
            o -= 1;
        }
        o
    };
    let (mut start, end) = {
        let (a, c) = (fix(anchor), fix(cursor));
        (a.min(c), a.max(c))
    };
    if backspace && start == end {
        start = text[..start]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }
    let insert = if backspace { "" } else { insert };
    let out = format!("{}{insert}{}", &text[..start], &text[end..]);
    (out, start + insert.len())
}

/// A short name for a MIDI device that is not plugged in, from its port
/// name ("client:port").
fn midi_name(id: &str) -> String {
    let port = id.split_once(':').map_or(id, |(_, p)| p);
    port.rsplit([')', ':'])
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(port)
        .to_string()
}

/// A MIDI port's name as shown in the options: without the ALSA
/// "client:port" numbers at the end, and without the device's name when the
/// port's name repeats it ("Scarlett 18i20 USB:Scarlett 18i20 USB MIDI 1
/// 24:0" becomes "Scarlett 18i20 USB MIDI 1").
fn port_label(name: &str) -> String {
    let mut s = name.trim();
    if let Some((head, tail)) = s.rsplit_once(' ') {
        let is_id = tail.split_once(':').is_some_and(|(a, b)| {
            !a.is_empty()
                && !b.is_empty()
                && (a.to_string() + b).chars().all(|c| c.is_ascii_digit())
        });
        if is_id {
            s = head.trim_end();
        }
    }
    match s.split_once(':') {
        Some((client, port)) if port.trim().starts_with(client.trim()) => port.trim().to_string(),
        Some((client, port)) if port.trim().is_empty() => client.trim().to_string(),
        Some((client, port)) => format!("{}: {}", client.trim(), port.trim()),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    #[test]
    fn port_labels() {
        assert_eq!(
            port_label("Scarlett 18i20 USB:Scarlett 18i20 USB MIDI 1 24:0"),
            "Scarlett 18i20 USB MIDI 1"
        );
        assert_eq!(
            port_label("ControlPad:ControlPad MIDI 1 32:0"),
            "ControlPad MIDI 1"
        );
        assert_eq!(
            port_label("MPK mini 3:MPK mini 3 MIDI 1 28:0"),
            "MPK mini 3 MIDI 1"
        );
        assert_eq!(port_label("Foo:Bar 20:1"), "Foo: Bar");
        assert_eq!(port_label("All inputs"), "All inputs");
    }

    #[test]
    fn on_screen_keys_edit_at_the_cursor() {
        // Typing at the cursor, in the middle.
        assert_eq!(edit_text("helo", 3, 3, "l", false), ("hello".into(), 4));
        // Typing over a selection (either direction).
        assert_eq!(edit_text("hello", 1, 4, "E", false), ("hEo".into(), 2));
        assert_eq!(edit_text("hello", 4, 1, "E", false), ("hEo".into(), 2));
        // Backspace deletes the character before the cursor, or the selection.
        assert_eq!(edit_text("hello", 5, 5, "", true), ("hell".into(), 4));
        assert_eq!(edit_text("hello", 0, 0, "", true), ("hello".into(), 0));
        assert_eq!(edit_text("hello", 0, 5, "", true), ("".into(), 0));
        // Multi-byte characters.
        assert_eq!(edit_text("aÄb", 3, 3, "", true), ("ab".into(), 1));
        // Offsets out of range are clamped.
        assert_eq!(edit_text("ab", 99, 99, "c", false), ("abc".into(), 3));
    }
}

// ============================================================== recording

impl App {
    /// Where new projects are made.
    fn projects_dir(&self) -> PathBuf {
        match self.settings.recording.projects_dir.trim() {
            "" => self.paths.default_projects_dir.clone(),
            d => expand_home(d),
        }
    }

    /// Why recording is not possible now, if it is not.
    fn recording_blocked(&self) -> Option<&'static str> {
        if !self.engine_running {
            Some("Recording needs the audio engine, which is not running.")
        } else if !self.engine_driver.to_ascii_lowercase().starts_with("jack") {
            Some("Recording needs the JACK driver (also PipeWire's): pick it in Options → Audio.")
        } else {
            None
        }
    }

    /// What to record of each track.
    fn track_specs(&self) -> Vec<TrackSpec> {
        let dry = self.settings.recording.record_dry;
        self.rig
            .tracks
            .iter()
            .map(|t| TrackSpec {
                id: t.id,
                name: t.name.clone(),
                dry_channels: if dry { t.input.channels().len() } else { 0 },
            })
            .collect()
    }

    /// Opens, rebuilds or closes the recorder's JACK client to match the
    /// project, the engine and the tracks. A take being recorded keeps the
    /// tracks it started with.
    fn sync_recorder(&mut self) {
        if self.recorder.as_ref().is_some_and(|r| r.recording()) {
            return;
        }
        if self.project.is_none() || self.recording_blocked().is_some() {
            self.drop_recorder();
            return;
        }
        let specs = self.track_specs();
        if let Some(r) = self.recorder.as_mut() {
            if r.fits(&specs) {
                r.set_tracks(specs);
                return;
            }
        }
        self.drop_recorder();
        match Recorder::open(specs) {
            Ok(r) => {
                self.engine
                    .send(Command::SetRecorder(Some(r.client_name().to_string())));
                self.recorder = Some(r);
            }
            Err(e) => self.set_rec_status(format!("The recorder could not start: {e:#}"), true),
        }
    }

    /// Closes the recorder, finishing a take that is being recorded.
    fn drop_recorder(&mut self) {
        let Some(mut r) = self.recorder.take() else {
            return;
        };
        if r.recording() {
            let dir = r.take_dir().map(Path::to_path_buf);
            match r.stop() {
                Ok(_) => self.set_rec_status(
                    format!(
                        "Recording stopped (the audio engine stopped); {} was saved.",
                        dir.as_deref().map(display_name).unwrap_or_default()
                    ),
                    true,
                ),
                Err(e) => self.set_rec_status(format!("The take could not be saved: {e:#}"), true),
            }
        }
        drop(r);
        self.engine.send(Command::SetRecorder(None));
    }

    fn open_project(&mut self, path: PathBuf) {
        if self.recorder.as_ref().is_some_and(|r| r.recording()) {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&path) {
            self.toast(format!("Could not create {}: {e}", path.display()), true);
            return;
        }
        self.set_rec_status(String::new(), false);
        self.project = Some(path);
        self.sync_recorder();
        self.refresh_recording();
    }

    fn close_project(&mut self) {
        self.drop_recorder();
        self.project = None;
        self.set_rec_status(String::new(), false);
        self.refresh_recording();
    }

    /// Starts a take, or goes on with a paused one.
    fn record(&mut self) {
        let Some(project) = self.project.clone() else {
            return;
        };
        if let Some(r) = &self.recorder {
            if r.recording() {
                r.set_paused(false);
                self.refresh_recording();
                return;
            }
        }
        if let Some(why) = self.recording_blocked() {
            self.toast(why, true);
            return;
        }
        self.sync_recorder();
        let Some(r) = self.recorder.as_mut() else {
            return;
        };
        let result = r.start(&project, self.settings.recording.format);
        match result {
            Ok(dir) => {
                self.rec_dropped = 0;
                self.set_rec_status(format!("Recording {}", display_name(&dir)), false);
            }
            Err(e) => self.set_rec_status(format!("Could not record: {e:#}"), true),
        }
        self.refresh_recording();
    }

    fn pause_recording(&mut self) {
        if let Some(r) = &self.recorder {
            if r.recording() {
                r.set_paused(true);
            }
        }
        self.refresh_recording();
    }

    fn stop_recording(&mut self) {
        let Some(r) = self.recorder.as_mut() else {
            return;
        };
        if !r.recording() {
            return;
        }
        let length = take_time(r.elapsed());
        let result = r.stop();
        match result {
            Ok(dir) => self.set_rec_status(
                format!(
                    "Saved {} ({length}) in {}",
                    display_name(&dir),
                    dir.display()
                ),
                false,
            ),
            Err(e) => self.set_rec_status(format!("The take could not be saved: {e:#}"), true),
        }
        // Tracks added or changed during the take are recorded from the
        // next one.
        self.sync_recorder();
        self.refresh_recording();
    }

    fn set_rec_status(&self, text: String, error: bool) {
        let ui = self.ui();
        let r = ui.global::<Recording>();
        r.set_status(ss(text));
        r.set_status_error(error);
    }

    /// Shows the take's length as it grows, and warns of gaps.
    fn show_take_time(&mut self) {
        let Some(r) = self.recorder.as_ref().filter(|r| r.recording()) else {
            return;
        };
        let time = take_time(r.elapsed());
        let dropped = r.dropped();
        if time != self.rec_time {
            self.rec_time = time.clone();
            self.ui().global::<Recording>().set_time(ss(time));
        }
        if dropped > self.rec_dropped {
            self.rec_dropped = dropped;
            self.set_rec_status(
                format!(
                    "The disk could not keep up: {dropped} short gap(s) in this take, in every track alike."
                ),
                true,
            );
        }
    }

    /// Everything the Record page and the record button show.
    fn refresh_recording(&mut self) {
        let ui = self.ui();
        let r = ui.global::<Recording>();
        let blocked = self.recording_blocked();
        r.set_available(blocked.is_none());
        r.set_unavailable_text(ss(blocked.unwrap_or("")));
        r.set_project(ss(self
            .project
            .as_deref()
            .map(display_name)
            .unwrap_or_default()));
        r.set_project_path(ss(self
            .project
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()));
        let location = self.projects_dir();
        r.set_location(ss(location.display().to_string()));
        r.set_projects(strings(list_projects(&location)));
        let state = match &self.recorder {
            Some(rec) if rec.recording() && rec.paused() => 2,
            Some(rec) if rec.recording() => 1,
            _ => 0,
        };
        r.set_state(state);
        if state == 0 {
            self.rec_time = take_time(Duration::ZERO);
            r.set_time(ss(&self.rec_time));
        }
        r.set_dry(self.settings.recording.record_dry);
        r.set_format(match self.settings.recording.format {
            RecordFormat::Wav => 0,
            RecordFormat::Flac => 1,
        });
    }
}

/// The last part of a path, for showing.
fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// "~/x" as a path below the home folder.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// The projects in `dir`: its folders, most recently changed first.
fn list_projects(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if name.starts_with('.') {
                return None;
            }
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, name))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.into_iter().map(|(_, n)| n).collect()
}

/// "02:14", or "1:02:14" past an hour.
fn take_time(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod recording_tests {
    use super::*;

    #[test]
    fn take_times() {
        assert_eq!(take_time(Duration::from_secs(0)), "00:00");
        assert_eq!(take_time(Duration::from_secs(134)), "02:14");
        assert_eq!(take_time(Duration::from_secs(3734)), "1:02:14");
    }

    #[test]
    fn home_is_expanded() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~/Music"), home.join("Music"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("/srv/x"), PathBuf::from("/srv/x"));
        assert_eq!(expand_home("~user/x"), PathBuf::from("~user/x"));
    }

    #[test]
    fn projects_are_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Old")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::create_dir(dir.path().join("New")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("file.txt"), "").unwrap();
        assert_eq!(list_projects(dir.path()), vec!["New", "Old"]);
    }
}
