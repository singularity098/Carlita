use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, GrabMode, ImageFormat, ImageOrder, InputFocus, MapState, ModMask, NotifyDetail,
    NotifyMode, StackMode, UnmapNotifyEvent, Window, WindowClass, UNMAP_NOTIFY_EVENT,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

/// How long to keep looking for an editor window after it was requested.
const FIND_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to look for requested windows (new windows also trigger a look).
const SEARCH_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for a window with the editor's title before taking a
/// new window from Carlita's own processes instead. Some editors do not use
/// the title Carla gives them.
const FALLBACK_AFTER: Duration = Duration::from_millis(1500);
/// How long to wait for the window manager to let go of an editor window.
const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(1);
/// How often to put an editor back if something (a window manager) takes it
/// out of Carlita's window again.
const MAX_REEMBED: u32 = 5;
/// The X keysym of F11.
const KEYSYM_F11: u32 = 0xffc8;
/// Presses of F11 closer together than this (key repeat) count once.
const KEY_REPEAT: Duration = Duration::from_millis(350);

/// The part of the main window an editor may use, in physical pixels
/// relative to the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The part both areas share (empty if none).
    pub fn intersect(&self, o: &Area) -> Area {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.width as i32).min(o.x + o.width as i32);
        let y1 = (self.y + self.height as i32).min(o.y + o.height as i32);
        if x1 <= x0 || y1 <= y0 {
            return Area::default();
        }
        Area {
            x: x0,
            y: y0,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        }
    }
}

pub enum HostCommand {
    /// Look for the window titled `title` and show it inside `parent` (the
    /// main window's X11 id). Send it before the editor is opened: if no
    /// window gets that title, a window that appears afterwards from
    /// Carlita's own processes is taken instead.
    Attach {
        key: u64,
        title: String,
        parent: u32,
    },
    /// Stop showing the editor and hand its window back.
    Detach {
        key: u64,
    },
    /// Where editors are shown (`area`), and the part of it nothing covers
    /// (`uncovered`): an editor is cut down to that part while a sidebar or
    /// the track bar slides over the area, and hidden when it is empty.
    SetArea {
        area: Area,
        uncovered: Area,
    },
    /// The main window (`parent`) and its edge strips. Invisible input-only
    /// windows are kept over the strips, above every editor, so touches and
    /// the mouse there reach the main window even where an editor lies
    /// under them (X passes the input on to the parent, which is the main
    /// window). F11 is also taken from the main window and everything in it,
    /// editors included ([`HostEvent::ToggleFullscreen`]).
    SetEdges {
        parent: u32,
        edges: Vec<Area>,
    },
    Shutdown,
}

pub enum HostEvent {
    /// The editor window was found and is shown in the main window.
    Attached {
        key: u64,
        width: u32,
        height: u32,
    },
    NotFound {
        key: u64,
    },
    /// The editor window went away (closed or crashed).
    Lost {
        key: u64,
    },
    /// A picture of the editor as it was just before something started to
    /// cover it, to draw where the editor is while it is cut down or hidden.
    /// `x` and `y` are in physical pixels relative to the main window;
    /// `rgba` holds `width * height` pixels.
    Snapshot {
        key: u64,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// F11 was pressed in Carlita's window, wherever the keyboard focus was
    /// (an editor that has the focus would otherwise keep the key).
    ToggleFullscreen,
    Unavailable(String),
}

/// Handle to the thread that embeds editor windows.
pub struct EditorHost {
    tx: Sender<HostCommand>,
    events: Receiver<HostEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EditorHost {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let thread = std::thread::Builder::new()
            .name("carlita-x11".into())
            .spawn(move || match Embedder::new(ev_tx.clone()) {
                Ok(e) => e.run(rx),
                Err(e) => {
                    log::warn!("editor embedding unavailable: {e:#}");
                    let _ = ev_tx.send(HostEvent::Unavailable(format!("{e:#}")));
                }
            })
            .expect("spawn x11 thread");
        Self {
            tx,
            events: ev_rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: HostCommand) {
        let _ = self.tx.send(cmd);
    }

    pub fn events(&self) -> &Receiver<HostEvent> {
        &self.events
    }
}

impl Drop for EditorHost {
    /// Hands every editor window back before returning, so none is destroyed
    /// together with the main window.
    fn drop(&mut self) {
        let _ = self.tx.send(HostCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Pending {
    title: String,
    parent: Window,
    since: Instant,
    /// The windows that were already shown when the editor was asked for:
    /// any other window that appears and belongs to Carlita (or one of its
    /// plugin bridges) is taken as the editor if none has its title.
    known: HashSet<Window>,
}

/// An editor window found and being taken back from the window manager.
struct Withdrawing {
    window: Window,
    parent: Window,
    since: Instant,
}

struct Embedded {
    /// The editor's window.
    window: Window,
    /// Our child of the main window that holds (and clips) it.
    holder: Window,
    /// The main window.
    parent: Window,
    width: u32,
    height: u32,
    reembeds: u32,
    /// While the holder is mapped: where the whole editor is, and the part
    /// of it the holder shows (the rest is under an overlay).
    shown: Option<(Area, Area)>,
}

struct Atoms {
    net_wm_name: u32,
    net_wm_pid: u32,
    utf8_string: u32,
    wm_state: u32,
}

struct Embedder {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    events: Sender<HostEvent>,
    pending: HashMap<u64, Pending>,
    withdrawing: HashMap<u64, Withdrawing>,
    embedded: HashMap<u64, Embedded>,
    area: Area,
    uncovered: Area,
    /// The main window the edge windows are in, and the edge windows.
    edge_parent: Option<Window>,
    edges: Vec<Window>,
    /// The key code of F11 taken from `edge_parent`, if it could be.
    f11: Option<u8>,
    last_f11: Option<Instant>,
}

impl Embedder {
    fn new(events: Sender<HostEvent>) -> Result<Self> {
        let (conn, screen) =
            x11rb::connect(None).context("cannot connect to the X server (is DISPLAY set?)")?;
        let root = conn.setup().roots[screen].root;
        let atom =
            |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let atoms = Atoms {
            net_wm_name: atom(b"_NET_WM_NAME")?,
            net_wm_pid: atom(b"_NET_WM_PID")?,
            utf8_string: atom(b"UTF8_STRING")?,
            wm_state: atom(b"WM_STATE")?,
        };
        // New and newly mapped windows trigger a search right away, so an
        // editor spends as little time as possible as a window of its own.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()
        .context("cannot watch for new windows")?;
        Ok(Self {
            conn,
            root,
            atoms,
            events,
            pending: HashMap::new(),
            withdrawing: HashMap::new(),
            embedded: HashMap::new(),
            area: Area::default(),
            uncovered: Area::default(),
            edge_parent: None,
            edges: Vec::new(),
            f11: None,
            last_f11: None,
        })
    }

    fn emit(&self, e: HostEvent) {
        let _ = self.events.send(e);
    }

    fn run(mut self, rx: Receiver<HostCommand>) {
        let mut last_search = Instant::now() - SEARCH_INTERVAL;
        loop {
            match rx.recv_timeout(Duration::from_millis(8)) {
                Ok(HostCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => {
                    if let Err(e) = self.handle(cmd) {
                        log::warn!("editor host: {e:#}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let new_windows = self.pump_x_events();
            if !self.pending.is_empty() && (new_windows || last_search.elapsed() > SEARCH_INTERVAL)
            {
                last_search = Instant::now();
                self.search_pending();
            }
            if !self.withdrawing.is_empty() {
                self.check_withdrawing();
            }
        }
        for key in self.embedded.keys().copied().collect::<Vec<_>>() {
            self.release(key);
        }
        let _ = self.conn.flush();
    }

    fn handle(&mut self, cmd: HostCommand) -> Result<()> {
        match cmd {
            HostCommand::Attach { key, title, parent } => {
                self.withdrawing.remove(&key);
                self.release(key);
                let known = self.client_windows().into_iter().collect();
                self.pending.insert(
                    key,
                    Pending {
                        title,
                        parent,
                        since: Instant::now(),
                        known,
                    },
                );
                // Carla may have shown the window already.
                self.search_pending();
            }
            HostCommand::Detach { key } => {
                self.pending.remove(&key);
                self.withdrawing.remove(&key);
                self.release(key);
            }
            HostCommand::SetArea { area, uncovered } => {
                let resized = area.width != self.area.width || area.height != self.area.height;
                self.area = area;
                self.uncovered = uncovered;
                for key in self.embedded.keys().copied().collect::<Vec<_>>() {
                    if resized {
                        self.fit_resizable(key);
                    }
                    self.place(key);
                }
            }
            HostCommand::SetEdges { parent, edges } => self.set_edges(parent, &edges)?,
            HostCommand::Shutdown => {}
        }
        self.conn.flush()?;
        Ok(())
    }

    // ------------------------------------------------------------- finding

    fn window_title(&self, w: Window) -> Option<String> {
        let net = self
            .conn
            .get_property(
                false,
                w,
                self.atoms.net_wm_name,
                self.atoms.utf8_string,
                0,
                256,
            )
            .ok()?
            .reply()
            .ok()?;
        if !net.value.is_empty() {
            return Some(String::from_utf8_lossy(&net.value).into_owned());
        }
        let wm = self
            .conn
            .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        (!wm.value.is_empty()).then(|| String::from_utf8_lossy(&wm.value).into_owned())
    }

    /// Whether the window has been shown. Carla sets an editor's size limits
    /// before showing it, so a window is only taken once it is mapped.
    fn is_mapped(&self, w: Window) -> bool {
        self.conn
            .get_window_attributes(w)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|a| a.map_state != MapState::UNMAPPED)
    }

    /// The editor's size limits, if it can be resized.
    fn size_limits(&self, w: Window) -> Option<((u32, u32), (u32, u32))> {
        let hints = WmSizeHints::get_normal_hints(&self.conn, w)
            .ok()?
            .reply()
            .ok()?;
        match hints {
            Some(h) => {
                let min = h
                    .min_size
                    .map_or((1, 1), |(w, h)| (w.max(1) as u32, h.max(1) as u32));
                let max = h
                    .max_size
                    .map_or((u32::MAX, u32::MAX), |(w, h)| (w as u32, h as u32));
                (min != max).then_some((min, max))
            }
            None => Some(((1, 1), (u32::MAX, u32::MAX))),
        }
    }

    /// Finds a window by title among the top-level windows and, below them,
    /// the window manager's frames.
    fn find_window(&self, root: Window, title: &str, depth: u32) -> Option<Window> {
        let tree = self.conn.query_tree(root).ok()?.reply().ok()?;
        for &child in &tree.children {
            if self.window_title(child).as_deref() == Some(title) && self.is_mapped(child) {
                return Some(child);
            }
        }
        if depth < 3 {
            for &child in &tree.children {
                if let Some(w) = self.find_window(child, title, depth + 1) {
                    return Some(w);
                }
            }
        }
        None
    }

    /// The windows the window manager shows (or would show) as windows of
    /// their own: the top-level windows, or the client windows inside the
    /// window manager's frames. Only those that are shown.
    fn client_windows(&self) -> Vec<Window> {
        let Some(tree) = self
            .conn
            .query_tree(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &top in &tree.children {
            let Some(attrs) = self
                .conn
                .get_window_attributes(top)
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                continue;
            };
            if attrs.override_redirect || attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            out.push(self.client_in(top, 0).unwrap_or(top));
        }
        out
    }

    /// The window with `WM_STATE` at or below `w` (a frame's client).
    fn client_in(&self, w: Window, depth: u32) -> Option<Window> {
        if self.has_wm_state(w) {
            return Some(w);
        }
        if depth >= 2 {
            return None;
        }
        let tree = self.conn.query_tree(w).ok()?.reply().ok()?;
        tree.children
            .iter()
            .find_map(|&c| self.client_in(c, depth + 1))
    }

    fn has_wm_state(&self, w: Window) -> bool {
        self.conn
            .get_property(false, w, self.atoms.wm_state, self.atoms.wm_state, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|p| !p.value.is_empty())
    }

    fn window_pid(&self, w: Window) -> Option<u32> {
        self.conn
            .get_property(false, w, self.atoms.net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    }

    /// A window that appeared after the editor was asked for and that
    /// belongs to Carlita or one of its child processes (plugin bridges,
    /// Wine), if there is exactly one: the editor of a plugin that ignores
    /// the title Carla gives its window.
    fn find_new_window(&self, p: &Pending) -> Option<Window> {
        let ours = own_processes();
        let taken: HashSet<Window> = self
            .embedded
            .values()
            .map(|e| e.window)
            .chain(self.withdrawing.values().map(|w| w.window))
            .collect();
        let candidates: Vec<Window> = self
            .client_windows()
            .into_iter()
            .filter(|w| !p.known.contains(w) && *w != p.parent && !taken.contains(w))
            .filter(|&w| {
                // Windows that say whose they are must be ours; most
                // plugin toolkits do not say.
                !matches!(self.window_pid(w), Some(pid) if !ours.contains(&pid))
            })
            .filter(|&w| {
                self.conn
                    .get_geometry(w)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|g| g.width >= 32 && g.height >= 32)
            })
            .collect();
        match candidates.as_slice() {
            [w] => {
                log::info!(
                    "no window titled {:?}; taking the new window {:#x} ({:?}) as the editor",
                    p.title,
                    w,
                    self.window_title(*w).unwrap_or_default()
                );
                Some(*w)
            }
            [] => None,
            more => {
                log::debug!(
                    "no window titled {:?}; {} new windows, not guessing",
                    p.title,
                    more.len()
                );
                None
            }
        }
    }

    fn search_pending(&mut self) {
        let keys: Vec<u64> = self.pending.keys().copied().collect();
        for key in keys {
            let p = &self.pending[&key];
            let found = self.find_window(self.root, &p.title, 0).or_else(|| {
                (p.since.elapsed() > FALLBACK_AFTER)
                    .then(|| self.find_new_window(p))
                    .flatten()
            });
            if let Some(w) = found {
                let parent = p.parent;
                self.pending.remove(&key);
                self.withdraw(key, w, parent);
            } else if p.since.elapsed() > FIND_TIMEOUT {
                self.pending.remove(&key);
                self.emit(HostEvent::NotFound { key });
            }
        }
    }

    // ----------------------------------------------------------- embedding

    fn is_managed(&self, w: Window) -> bool {
        let has_state = self
            .conn
            .get_property(false, w, self.atoms.wm_state, self.atoms.wm_state, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|p| p.value32().and_then(|mut v| v.next()))
            // 0 = WithdrawnState.
            .is_some_and(|state| state != 0);
        let parent = self
            .conn
            .query_tree(w)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|t| t.parent);
        has_state || parent.is_some_and(|p| p != self.root)
    }

    /// Takes a found editor window away from the window manager, the way a
    /// program withdraws its own window (ICCCM 4.1.4): unmap it and tell the
    /// window manager. Otherwise the window manager keeps treating the
    /// window as its own and may unmap or move it later.
    fn withdraw(&mut self, key: u64, window: Window, parent: Window) {
        if self.is_managed(window) {
            let _ = self.conn.unmap_window(window);
            let ev = UnmapNotifyEvent {
                response_type: UNMAP_NOTIFY_EVENT,
                sequence: 0,
                event: self.root,
                window,
                from_configure: false,
            };
            let _ = self.conn.send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            );
            let _ = self.conn.flush();
        }
        self.withdrawing.insert(
            key,
            Withdrawing {
                window,
                parent,
                since: Instant::now(),
            },
        );
        self.check_withdrawing();
    }

    /// Embeds the editors the window manager has let go of.
    fn check_withdrawing(&mut self) {
        let keys: Vec<u64> = self.withdrawing.keys().copied().collect();
        for key in keys {
            let w = &self.withdrawing[&key];
            if self.is_managed(w.window) && w.since.elapsed() < WITHDRAW_TIMEOUT {
                continue;
            }
            let w = self.withdrawing.remove(&key).unwrap();
            if let Err(e) = self.embed(key, w.window, w.parent) {
                log::warn!("cannot embed editor window: {e:#}");
                self.emit(HostEvent::NotFound { key });
            }
        }
    }

    fn embed(&mut self, key: u64, window: Window, parent: Window) -> Result<()> {
        let geo = self.conn.get_geometry(window)?.reply()?;
        let holder = self.conn.generate_id()?;
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                holder,
                parent,
                0,
                0,
                (geo.width as u32).max(1) as u16,
                (geo.height as u32).max(1) as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                // Leaving the editor gives the keyboard back to Carlita.
                &CreateWindowAux::new()
                    .background_pixel(0)
                    .event_mask(EventMask::LEAVE_WINDOW),
            )?
            .check()
            .context("cannot create the editor's holder window")?;
        // The new holder is on top: keep the edge strips above it.
        self.raise_edges();
        self.reembed(window, holder);
        self.embedded.insert(
            key,
            Embedded {
                window,
                holder,
                parent,
                width: geo.width as u32,
                height: geo.height as u32,
                reembeds: 0,
                shown: None,
            },
        );
        self.fit_resizable(key);
        self.place(key);
        self.conn.flush()?;
        self.emit(HostEvent::Attached {
            key,
            width: geo.width as u32,
            height: geo.height as u32,
        });
        Ok(())
    }

    /// Moves the editor into its holder, without hearing about it (the
    /// move unmaps the window, which would look like the editor closing).
    fn reembed(&self, window: Window, holder: Window) {
        let mask = |m| ChangeWindowAttributesAux::new().event_mask(m);
        let _ = self
            .conn
            .change_window_attributes(window, &mask(EventMask::NO_EVENT));
        let _ = self.conn.reparent_window(window, holder, 0, 0);
        let _ = self.conn.map_window(window);
        let _ = self
            .conn
            .change_window_attributes(window, &mask(EventMask::STRUCTURE_NOTIFY));
    }

    /// Gives an editor that can resize the whole area (within its limits).
    fn fit_resizable(&mut self, key: u64) {
        let Some(e) = self.embedded.get(&key) else {
            return;
        };
        // Read every time: a plugin may change its mind.
        let Some((min, max)) = self.size_limits(e.window) else {
            return;
        };
        if self.area.width == 0 || self.area.height == 0 {
            return;
        }
        let w = self.area.width.clamp(min.0, max.0.max(min.0));
        let h = self.area.height.clamp(min.1, max.1.max(min.1));
        if (w, h) != (e.width, e.height) {
            let _ = self
                .conn
                .configure_window(e.window, &ConfigureWindowAux::new().width(w).height(h));
        }
    }

    /// Puts the editor in the middle of the area, as large as it is but no
    /// larger than the area (a larger editor is cut off at the right and
    /// bottom). The holder covers only the part of it nothing else covers,
    /// with the editor moved inside it so it stays where it is; when nothing
    /// is left, the holder is hidden. Just before an overlay starts to cover
    /// the whole editor, a snapshot of it is sent to draw in its place.
    fn place(&mut self, key: u64) {
        let Some(e) = self.embedded.get(&key) else {
            return;
        };
        let a = self.area;
        let full = if a.is_empty() {
            Area::default()
        } else {
            let w = e.width.min(a.width).max(1);
            let h = e.height.min(a.height).max(1);
            Area {
                x: a.x + (a.width - w) as i32 / 2,
                y: a.y + (a.height - h) as i32 / 2,
                width: w,
                height: h,
            }
        };
        let part = full.intersect(&self.uncovered);
        if e.shown == Some((full, full)) && part != full {
            self.snapshot(key, full);
        }
        if part.is_empty() {
            let e = self.embedded.get_mut(&key).unwrap();
            if e.shown.take().is_some() {
                let holder = e.holder;
                self.take_focus_from(holder);
                let _ = self.conn.unmap_window(holder);
            }
            return;
        }
        let e = self.embedded.get_mut(&key).unwrap();
        if e.shown.map(|s| s.1) != Some(part) || e.shown.map(|s| s.0) != Some(full) {
            let _ = self.conn.configure_window(
                e.window,
                &ConfigureWindowAux::new()
                    .x(full.x - part.x)
                    .y(full.y - part.y),
            );
            let _ = self.conn.configure_window(
                e.holder,
                &ConfigureWindowAux::new()
                    .x(part.x)
                    .y(part.y)
                    .width(part.width)
                    .height(part.height),
            );
        }
        if e.shown.is_none() {
            let _ = self.conn.map_window(e.holder);
        }
        e.shown = Some((full, part));
    }

    /// If the keyboard focus is in the editor (the user clicked into it),
    /// gives it back to the main window before the editor is hidden, so
    /// typing reaches Carlita (the text prompt) rather than nowhere.
    fn take_focus_from(&self, holder: Window) {
        let Some(focus) = self
            .conn
            .get_input_focus()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|f| f.focus)
        else {
            return;
        };
        let mut w = focus;
        for _ in 0..16 {
            if w == holder {
                let main = self
                    .conn
                    .query_tree(holder)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|t| t.parent);
                if let Some(main) = main {
                    let _ =
                        self.conn
                            .set_input_focus(InputFocus::PARENT, main, x11rb::CURRENT_TIME);
                }
                return;
            }
            match self.conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                Some(t) if t.parent != 0 && w != self.root => w = t.parent,
                _ => return,
            }
        }
    }

    /// If the keyboard focus went nowhere (an editor that had it was closed,
    /// and X let the focus go with it), gives it to the main window. The
    /// main window would otherwise take no typing until it is focused anew,
    /// which clicking into it does not do while the window manager thinks
    /// it already has the focus.
    fn keep_focus(&self, main: Window) {
        let focus = self
            .conn
            .get_input_focus()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|f| f.focus);
        // 0 is None (no focus at all).
        if focus == Some(0) {
            let _ = self
                .conn
                .set_input_focus(InputFocus::PARENT, main, x11rb::CURRENT_TIME);
        }
    }

    /// Reads what the editor shows (it is uncovered, see `place`) and sends
    /// it as RGBA.
    fn snapshot(&self, key: u64, at: Area) {
        let Some(e) = self.embedded.get(&key) else {
            return;
        };
        match self.read_window(e.window, at.width, at.height) {
            Ok(rgba) => self.emit(HostEvent::Snapshot {
                key,
                x: at.x,
                y: at.y,
                width: at.width,
                height: at.height,
                rgba,
            }),
            Err(err) => log::debug!("no editor snapshot: {err:#}"),
        }
    }

    fn read_window(&self, w: Window, width: u32, height: u32) -> Result<Vec<u8>> {
        let attrs = self.conn.get_window_attributes(w)?.reply()?;
        let img = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                w,
                0,
                0,
                width as u16,
                height as u16,
                !0,
            )?
            .reply()?;
        let setup = self.conn.setup();
        let visual = setup
            .roots
            .iter()
            .flat_map(|s| &s.allowed_depths)
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == attrs.visual)
            .context("unknown visual")?;
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == img.depth)
            .map(|f| f.bits_per_pixel)
            .context("unknown pixmap format")?;
        if bpp != 32 || img.depth < 24 {
            anyhow::bail!("unsupported pixel format (depth {}, {bpp} bpp)", img.depth);
        }
        let n = (width * height) as usize;
        if img.data.len() < n * 4 {
            anyhow::bail!("short image");
        }
        let lsb = setup.image_byte_order == ImageOrder::LSB_FIRST;
        let channel = |p: u32, mask: u32| -> u8 {
            if mask == 0 {
                return 0;
            }
            ((p & mask) >> mask.trailing_zeros()) as u8
        };
        let mut out = Vec::with_capacity(n * 4);
        for px in img.data[..n * 4].chunks_exact(4) {
            let b = [px[0], px[1], px[2], px[3]];
            let p = if lsb {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            };
            out.extend_from_slice(&[
                channel(p, visual.red_mask),
                channel(p, visual.green_mask),
                channel(p, visual.blue_mask),
                255,
            ]);
        }
        Ok(out)
    }

    /// Hands the editor window back as an unmapped top-level window (so it
    /// is not destroyed with the main window) and removes the holder.
    fn release(&mut self, key: u64) {
        if let Some(e) = self.embedded.remove(&key) {
            self.take_focus_from(e.holder);
            let _ = self.conn.change_window_attributes(
                e.window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            );
            let _ = self.conn.unmap_window(e.window);
            let _ = self.conn.reparent_window(e.window, self.root, 0, 0);
            let _ = self.conn.destroy_window(e.holder);
            self.keep_focus(e.parent);
            let _ = self.conn.flush();
        }
    }

    /// The editor window is gone or was closed: drop it without touching it.
    fn lost(&mut self, key: u64) {
        if let Some(e) = self.embedded.remove(&key) {
            self.take_focus_from(e.holder);
            let _ = self.conn.destroy_window(e.holder);
            self.keep_focus(e.parent);
            self.emit(HostEvent::Lost { key });
        }
    }

    // ---------------------------------------------------------- edge strips

    fn set_edges(&mut self, parent: Window, areas: &[Area]) -> Result<()> {
        if self.edge_parent != Some(parent) {
            for w in self.edges.drain(..) {
                let _ = self.conn.destroy_window(w);
            }
            self.edge_parent = Some(parent);
            self.f11 = self.grab_f11(parent);
        }
        while self.edges.len() > areas.len() {
            let w = self.edges.pop().unwrap();
            let _ = self.conn.destroy_window(w);
        }
        while self.edges.len() < areas.len() {
            let w = self.conn.generate_id()?;
            self.conn
                .create_window(
                    0,
                    w,
                    parent,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    x11rb::COPY_FROM_PARENT,
                    &CreateWindowAux::new(),
                )?
                .check()
                .context("cannot create an edge window")?;
            self.edges.push(w);
        }
        for (&w, a) in self.edges.iter().zip(areas) {
            if a.is_empty() {
                let _ = self.conn.unmap_window(w);
                continue;
            }
            let _ = self.conn.configure_window(
                w,
                &ConfigureWindowAux::new()
                    .x(a.x)
                    .y(a.y)
                    .width(a.width)
                    .height(a.height)
                    .stack_mode(StackMode::ABOVE),
            );
            let _ = self.conn.map_window(w);
        }
        Ok(())
    }

    fn raise_edges(&self) {
        for &w in &self.edges {
            let _ = self
                .conn
                .configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
    }

    /// Takes F11 from `parent` and everything in it, with any modifiers, so
    /// it reaches Carlita even while an editor has the keyboard focus.
    fn grab_f11(&self, parent: Window) -> Option<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = map.keysyms_per_keycode.max(1) as usize;
        let index = map
            .keysyms
            .chunks(per)
            .position(|syms| syms.contains(&KEYSYM_F11))?;
        let code = min + index as u8;
        match self
            .conn
            .grab_key(
                false,
                parent,
                ModMask::ANY,
                code,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map(|c| c.check())
        {
            Ok(Ok(())) => Some(code),
            Ok(Err(e)) => {
                log::info!("cannot take F11 from the main window: {e}");
                None
            }
            Err(e) => {
                log::info!("cannot take F11 from the main window: {e}");
                None
            }
        }
    }

    fn holder_key(&self, w: Window) -> Option<u64> {
        self.embedded
            .iter()
            .find(|(_, e)| e.holder == w)
            .map(|(k, _)| *k)
    }

    fn key_for_window(&self, w: Window) -> Option<u64> {
        self.embedded
            .iter()
            .find(|(_, e)| e.window == w)
            .map(|(k, _)| *k)
    }

    /// Handles X events; returns whether a window appeared.
    fn pump_x_events(&mut self) -> bool {
        let mut new_windows = false;
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                XEvent::CreateNotify(_) | XEvent::MapNotify(_) => new_windows = true,
                XEvent::KeyPress(k) if Some(k.detail) == self.f11 => {
                    if self.last_f11.is_none_or(|t| t.elapsed() > KEY_REPEAT) {
                        self.emit(HostEvent::ToggleFullscreen);
                    }
                    self.last_f11 = Some(Instant::now());
                }
                // The pointer left an editor for the rest of Carlita: give
                // the keyboard back to the main window, so typing reaches
                // Carlita's own text fields again (an editor that took the
                // focus, as yabridge's do on hover, keeps it otherwise).
                XEvent::LeaveNotify(l)
                    if l.detail != NotifyDetail::INFERIOR
                        && (l.mode == NotifyMode::NORMAL || l.mode == NotifyMode::UNGRAB) =>
                {
                    if self.holder_key(l.event).is_some() {
                        self.take_focus_from(l.event);
                        let _ = self.conn.flush();
                    }
                }
                // Events about the editor itself (not the root's children).
                XEvent::ConfigureNotify(c) if c.event == c.window => {
                    if let Some(k) = self.key_for_window(c.window) {
                        let e = self.embedded.get_mut(&k).unwrap();
                        if (e.width, e.height) != (c.width as u32, c.height as u32) {
                            e.width = c.width as u32;
                            e.height = c.height as u32;
                            self.place(k);
                        }
                    }
                }
                XEvent::DestroyNotify(d) if d.event == d.window => {
                    if let Some(k) = self.key_for_window(d.window) {
                        self.lost(k);
                    }
                }
                XEvent::UnmapNotify(u) if u.event == u.window => {
                    // The plugin closed its editor (and did not just get
                    // reparented, which unmaps the window too).
                    if let Some(k) = self.key_for_window(u.window) {
                        let holder = self.embedded[&k].holder;
                        let parent = self
                            .conn
                            .query_tree(u.window)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|t| t.parent);
                        if parent == Some(holder) {
                            self.release(k);
                            self.emit(HostEvent::Lost { key: k });
                        }
                    }
                }
                XEvent::ReparentNotify(r) if r.event == r.window => {
                    // Something (a window manager tidying up) took the editor
                    // out of the holder: put it back.
                    if let Some(k) = self.key_for_window(r.window) {
                        let e = self.embedded.get_mut(&k).unwrap();
                        if r.parent != e.holder {
                            e.reembeds += 1;
                            if e.reembeds > MAX_REEMBED {
                                log::warn!("editor window keeps being taken away; giving up");
                                self.release(k);
                                self.emit(HostEvent::Lost { key: k });
                            } else {
                                let (w, h) = (e.window, e.holder);
                                self.reembed(w, h);
                                // The editor is back at the holder's corner:
                                // move it to where it belongs.
                                let e = self.embedded.get_mut(&k).unwrap();
                                if let Some((full, _)) = e.shown {
                                    e.shown = Some((full, Area::default()));
                                }
                                self.place(k);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = self.conn.flush();
        new_windows
    }
}

/// Carlita's process id and those of all its descendants (plugin bridges,
/// the Wine processes of yabridge plugins), read from /proc.
fn own_processes() -> HashSet<u32> {
    let me = std::process::id();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "pid (name) state ppid ...": the name may hold spaces and
            // parentheses, so read after the last ')'.
            let ppid = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|p| p.parse::<u32>().ok());
            if let Some(ppid) = ppid {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut out = HashSet::from([me]);
    let mut todo = vec![me];
    while let Some(p) = todo.pop() {
        for &c in children.get(&p).into_iter().flatten() {
            if out.insert(c) {
                todo.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_processes_include_children() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let ours = own_processes();
        let _ = child.kill();
        let _ = child.wait();
        assert!(ours.contains(&std::process::id()));
        assert!(ours.contains(&child.id()));
        assert!(!ours.contains(&1) || std::process::id() == 1);
    }
}
