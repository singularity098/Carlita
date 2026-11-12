use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, ImageFormat, ImageOrder, MapState, UnmapNotifyEvent, Window, WindowClass,
    UNMAP_NOTIFY_EVENT,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

/// How long to keep looking for an editor window after it was requested.
const FIND_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to look for requested windows (new windows also trigger a look).
const SEARCH_INTERVAL: Duration = Duration::from_millis(100);
/// How long to wait for the window manager to let go of an editor window.
const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(1);
/// How often to put an editor back if something (a window manager) takes it
/// out of Carlita's window again.
const MAX_REEMBED: u32 = 5;

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
    /// main window's X11 id).
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
    width: u32,
    height: u32,
    reembeds: u32,
    /// While the holder is mapped: where the whole editor is, and the part
    /// of it the holder shows (the rest is under an overlay).
    shown: Option<(Area, Area)>,
}

struct Atoms {
    net_wm_name: u32,
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
                self.pending.insert(
                    key,
                    Pending {
                        title,
                        parent,
                        since: Instant::now(),
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

    fn search_pending(&mut self) {
        let keys: Vec<u64> = self.pending.keys().copied().collect();
        for key in keys {
            let p = &self.pending[&key];
            if let Some(w) = self.find_window(self.root, &p.title, 0) {
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
                &CreateWindowAux::new().background_pixel(0),
            )?
            .check()
            .context("cannot create the editor's holder window")?;
        self.reembed(window, holder);
        self.embedded.insert(
            key,
            Embedded {
                window,
                holder,
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
        let e = self.embedded.get_mut(&key).unwrap();
        if part.is_empty() {
            if e.shown.take().is_some() {
                let _ = self.conn.unmap_window(e.holder);
            }
            return;
        }
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
            let _ = self.conn.change_window_attributes(
                e.window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            );
            let _ = self.conn.unmap_window(e.window);
            let _ = self.conn.reparent_window(e.window, self.root, 0, 0);
            let _ = self.conn.destroy_window(e.holder);
            let _ = self.conn.flush();
        }
    }

    /// The editor window is gone or was closed: drop it without touching it.
    fn lost(&mut self, key: u64) {
        if let Some(e) = self.embedded.remove(&key) {
            let _ = self.conn.destroy_window(e.holder);
            self.emit(HostEvent::Lost { key });
        }
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
