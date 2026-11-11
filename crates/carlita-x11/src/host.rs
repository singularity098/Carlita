use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::xproto::{
    AtomEnum, ButtonPressEvent, ButtonReleaseEvent, ChangeWindowAttributesAux, ConfigureWindowAux,
    ConnectionExt as _, EventMask, ImageFormat, KeyButMask, Motion, MotionNotifyEvent, StackMode,
    Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

/// How long to keep looking for an editor window after it was requested.
const FIND_TIMEOUT: Duration = Duration::from_secs(15);
/// Frame rate cap for captured editors.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Capture at least this often even without damage events (some GL editors
/// do not generate damage).
const IDLE_REFRESH: Duration = Duration::from_millis(500);
/// Off-screen position of captured editors.
const PARK_X: i32 = -16000;

/// One captured editor frame, RGBA8, tightly packed.
pub struct EditorFrame {
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    Down,
    Up,
    Move,
    ScrollUp,
    ScrollDown,
}

/// How pointer input reaches the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// Synthetic events with XSendEvent (no visible side effects).
    #[default]
    SendEvent,
    /// Real input through XTest: while a finger is down the editor is raised
    /// at the screen origin, unscaled, and real pointer events are injected.
    /// Works with toolkits that ignore synthetic events, at the cost of the
    /// unscaled editor being visible during the gesture.
    XTest,
}

pub enum HostCommand {
    /// Start looking for the window titled `title` and capture it.
    Attach {
        key: u64,
        title: String,
    },
    Detach {
        key: u64,
    },
    /// Pointer event in editor coordinates.
    Pointer {
        key: u64,
        kind: PointerKind,
        x: i32,
        y: i32,
    },
    /// Ask a resizable editor to take a new size.
    Resize {
        key: u64,
        width: u32,
        height: u32,
    },
    SetInputMode(InputMode),
    Shutdown,
}

pub enum HostEvent {
    /// The editor window was found and is being captured.
    Attached {
        key: u64,
        width: u32,
        height: u32,
        resizable: bool,
    },
    NotFound {
        key: u64,
    },
    Frame(EditorFrame),
    /// The editor window went away (closed or crashed).
    Lost {
        key: u64,
    },
    Unavailable(String),
}

/// Handle to the capture thread.
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
            .spawn(move || match Capture::new(ev_tx.clone()) {
                Ok(c) => c.run(rx),
                Err(e) => {
                    log::warn!("editor capture unavailable: {e:#}");
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
    fn drop(&mut self) {
        let _ = self.tx.send(HostCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Pending {
    title: String,
    since: Instant,
}

struct Captured {
    window: Window,
    damage: u32,
    width: u32,
    height: u32,
    dirty: bool,
    last_frame: Instant,
    /// Where the window was before we parked it.
    original: (i16, i16),
}

struct Atoms {
    net_wm_name: u32,
    utf8_string: u32,
}

struct Capture {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    events: Sender<HostEvent>,
    pending: HashMap<u64, Pending>,
    captured: HashMap<u64, Captured>,
    input_mode: InputMode,
    button_down: bool,
}

impl Capture {
    fn new(events: Sender<HostEvent>) -> Result<Self> {
        let (conn, screen) =
            x11rb::connect(None).context("cannot connect to the X server (is DISPLAY set?)")?;
        let root = conn.setup().roots[screen].root;
        conn.composite_query_version(0, 4)?
            .reply()
            .context("XComposite extension missing")?;
        conn.damage_query_version(1, 1)?
            .reply()
            .context("XDamage extension missing")?;
        let atom =
            |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let atoms = Atoms {
            net_wm_name: atom(b"_NET_WM_NAME")?,
            utf8_string: atom(b"UTF8_STRING")?,
        };
        Ok(Self {
            conn,
            root,
            atoms,
            events,
            pending: HashMap::new(),
            captured: HashMap::new(),
            input_mode: InputMode::default(),
            button_down: false,
        })
    }

    fn emit(&self, e: HostEvent) {
        let _ = self.events.send(e);
    }

    fn run(mut self, rx: Receiver<HostCommand>) {
        let mut last_search = Instant::now() - Duration::from_secs(1);
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
            if !self.pending.is_empty() && last_search.elapsed() > Duration::from_millis(100) {
                last_search = Instant::now();
                self.search_pending();
            }
            self.pump_x_events();
            self.capture_dirty();
        }
        for key in self.captured.keys().copied().collect::<Vec<_>>() {
            self.release(key);
        }
        let _ = self.conn.flush();
    }

    fn handle(&mut self, cmd: HostCommand) -> Result<()> {
        match cmd {
            HostCommand::Attach { key, title } => {
                self.release(key);
                self.pending.insert(
                    key,
                    Pending {
                        title,
                        since: Instant::now(),
                    },
                );
            }
            HostCommand::Detach { key } => {
                self.pending.remove(&key);
                self.release(key);
            }
            HostCommand::Pointer { key, kind, x, y } => self.pointer(key, kind, x, y)?,
            HostCommand::Resize { key, width, height } => {
                if let Some(c) = self.captured.get(&key) {
                    self.conn.configure_window(
                        c.window,
                        &ConfigureWindowAux::new().width(width).height(height),
                    )?;
                    self.conn.flush()?;
                }
            }
            HostCommand::SetInputMode(m) => self.input_mode = m,
            HostCommand::Shutdown => {}
        }
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

    fn find_window(&self, root: Window, title: &str, depth: u32) -> Option<Window> {
        let tree = self.conn.query_tree(root).ok()?.reply().ok()?;
        for &child in &tree.children {
            if self.window_title(child).as_deref() == Some(title) {
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
                self.pending.remove(&key);
                if let Err(e) = self.start_capture(key, w) {
                    log::warn!("cannot capture editor window: {e:#}");
                    self.emit(HostEvent::NotFound { key });
                }
            } else if p.since.elapsed() > FIND_TIMEOUT {
                self.pending.remove(&key);
                self.emit(HostEvent::NotFound { key });
            }
        }
    }

    // ------------------------------------------------------------- capture

    fn start_capture(&mut self, key: u64, window: Window) -> Result<()> {
        let geo = self.conn.get_geometry(window)?.reply()?;
        let resizable = match WmSizeHints::get_normal_hints(&self.conn, window)?.reply()? {
            Some(h) => match (h.min_size, h.max_size) {
                (Some(min), Some(max)) => min != max,
                _ => true,
            },
            None => true,
        };
        // Keep the window mapped (so it keeps drawing) but off-screen, and
        // ask the server to render it into an off-screen pixmap.
        self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        self.conn
            .configure_window(window, &ConfigureWindowAux::new().x(PARK_X).y(0))?;
        self.conn
            .composite_redirect_window(window, Redirect::AUTOMATIC)?;
        let damage = self.conn.generate_id()?;
        self.conn
            .damage_create(damage, window, ReportLevel::NON_EMPTY)?;
        self.conn.flush()?;

        self.captured.insert(
            key,
            Captured {
                window,
                damage,
                width: geo.width as u32,
                height: geo.height as u32,
                dirty: true,
                last_frame: Instant::now() - FRAME_INTERVAL,
                original: (geo.x, geo.y),
            },
        );
        self.emit(HostEvent::Attached {
            key,
            width: geo.width as u32,
            height: geo.height as u32,
            resizable,
        });
        Ok(())
    }

    fn release(&mut self, key: u64) {
        if let Some(c) = self.captured.remove(&key) {
            let _ = self.conn.damage_destroy(c.damage);
            let _ = self
                .conn
                .composite_unredirect_window(c.window, Redirect::AUTOMATIC);
            let _ = self.conn.configure_window(
                c.window,
                &ConfigureWindowAux::new()
                    .x(c.original.0 as i32)
                    .y(c.original.1 as i32),
            );
            let _ = self.conn.flush();
        }
    }

    fn key_for_window(&self, w: Window) -> Option<u64> {
        self.captured
            .iter()
            .find(|(_, c)| c.window == w)
            .map(|(k, _)| *k)
    }

    fn key_for_damage(&self, d: u32) -> Option<u64> {
        self.captured
            .iter()
            .find(|(_, c)| c.damage == d)
            .map(|(k, _)| *k)
    }

    fn pump_x_events(&mut self) {
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            match ev {
                XEvent::DamageNotify(d) => {
                    if let Some(k) = self.key_for_damage(d.damage) {
                        self.captured.get_mut(&k).unwrap().dirty = true;
                    }
                    let _ = self
                        .conn
                        .damage_subtract(d.damage, x11rb::NONE, x11rb::NONE);
                }
                XEvent::ConfigureNotify(c) => {
                    if let Some(k) = self.key_for_window(c.window) {
                        let cap = self.captured.get_mut(&k).unwrap();
                        cap.width = c.width as u32;
                        cap.height = c.height as u32;
                        cap.dirty = true;
                        // Window managers may move it back; park it again.
                        if c.x as i32 != PARK_X
                            && self.input_mode == InputMode::SendEvent
                            && !self.button_down
                        {
                            let _ = self.conn.configure_window(
                                c.window,
                                &ConfigureWindowAux::new().x(PARK_X).y(0),
                            );
                        }
                    }
                }
                XEvent::DestroyNotify(d) => {
                    if let Some(k) = self.key_for_window(d.window) {
                        self.captured.remove(&k);
                        self.emit(HostEvent::Lost { key: k });
                    }
                }
                XEvent::UnmapNotify(u) => {
                    if let Some(k) = self.key_for_window(u.window) {
                        self.release(k);
                        self.emit(HostEvent::Lost { key: k });
                    }
                }
                _ => {}
            }
        }
        let _ = self.conn.flush();
    }

    fn capture_dirty(&mut self) {
        let keys: Vec<u64> = self.captured.keys().copied().collect();
        for key in keys {
            let c = &self.captured[&key];
            let due = (c.dirty && c.last_frame.elapsed() >= FRAME_INTERVAL)
                || c.last_frame.elapsed() >= IDLE_REFRESH;
            if !due {
                continue;
            }
            match self.grab(c.window, c.width, c.height) {
                Ok(rgba) => {
                    let c = self.captured.get_mut(&key).unwrap();
                    c.dirty = false;
                    c.last_frame = Instant::now();
                    let (width, height) = (c.width, c.height);
                    self.emit(HostEvent::Frame(EditorFrame {
                        key,
                        width,
                        height,
                        rgba,
                    }));
                }
                Err(e) => {
                    log::debug!("capture failed: {e:#}");
                    self.captured.get_mut(&key).unwrap().last_frame = Instant::now();
                }
            }
        }
    }

    /// Copies the window's off-screen pixmap and converts BGRX to RGBA.
    fn grab(&self, window: Window, width: u32, height: u32) -> Result<Vec<u8>> {
        let pixmap = self.conn.generate_id()?;
        self.conn.composite_name_window_pixmap(window, pixmap)?;
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                0,
                0,
                width as u16,
                height as u16,
                !0,
            )?
            .reply();
        let _ = self.conn.free_pixmap(pixmap);
        let image = image?;
        let px = (width * height) as usize;
        let data = image.data;
        if data.len() < px * 4 {
            anyhow::bail!("unexpected image format (depth {})", image.depth);
        }
        let mut rgba = Vec::with_capacity(px * 4);
        for p in data.chunks_exact(4).take(px) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        Ok(rgba)
    }

    // --------------------------------------------------------------- input

    /// Finds the deepest child of `top` containing (x, y) (relative to `top`).
    fn target_at(&self, top: Window, x: i32, y: i32) -> (Window, i16, i16) {
        let (mut win, mut wx, mut wy) = (top, x as i16, y as i16);
        for _ in 0..16 {
            let Ok(Ok(r)) = self
                .conn
                .translate_coordinates(win, win, wx, wy)
                .map(|c| c.reply())
            else {
                break;
            };
            if r.child == x11rb::NONE {
                break;
            }
            let Ok(Ok(t)) = self
                .conn
                .translate_coordinates(win, r.child, wx, wy)
                .map(|c| c.reply())
            else {
                break;
            };
            win = r.child;
            wx = t.dst_x;
            wy = t.dst_y;
        }
        (win, wx, wy)
    }

    fn pointer(&mut self, key: u64, kind: PointerKind, x: i32, y: i32) -> Result<()> {
        let Some(c) = self.captured.get(&key) else {
            return Ok(());
        };
        let top = c.window;
        match kind {
            PointerKind::Down => self.button_down = true,
            PointerKind::Up => self.button_down = false,
            _ => {}
        }
        if self.input_mode == InputMode::XTest {
            return self.pointer_xtest(key, top, kind, x, y);
        }

        let (target, ex, ey) = self.target_at(top, x, y);
        let origin = self
            .conn
            .translate_coordinates(top, self.root, 0, 0)?
            .reply()?;
        let (rx, ry) = (origin.dst_x + x as i16, origin.dst_y + y as i16);
        let state = if self.button_down {
            KeyButMask::BUTTON1
        } else {
            KeyButMask::default()
        };
        let time = x11rb::CURRENT_TIME;

        match kind {
            PointerKind::Move => {
                let ev = MotionNotifyEvent {
                    response_type: MOTION_NOTIFY_EVENT,
                    detail: Motion::NORMAL,
                    sequence: 0,
                    time,
                    root: self.root,
                    event: target,
                    child: x11rb::NONE,
                    root_x: rx,
                    root_y: ry,
                    event_x: ex,
                    event_y: ey,
                    state,
                    same_screen: true,
                };
                let mask = if self.button_down {
                    EventMask::BUTTON_MOTION | EventMask::POINTER_MOTION
                } else {
                    EventMask::POINTER_MOTION
                };
                self.conn.send_event(true, target, mask, ev)?;
            }
            PointerKind::Down | PointerKind::ScrollUp | PointerKind::ScrollDown => {
                let button = match kind {
                    PointerKind::ScrollUp => 4,
                    PointerKind::ScrollDown => 5,
                    _ => 1,
                };
                let press = ButtonPressEvent {
                    response_type: BUTTON_PRESS_EVENT,
                    detail: button,
                    sequence: 0,
                    time,
                    root: self.root,
                    event: target,
                    child: x11rb::NONE,
                    root_x: rx,
                    root_y: ry,
                    event_x: ex,
                    event_y: ey,
                    state: KeyButMask::default(),
                    same_screen: true,
                };
                self.conn
                    .send_event(true, target, EventMask::BUTTON_PRESS, press)?;
                if button != 1 {
                    let release = ButtonReleaseEvent {
                        response_type: BUTTON_RELEASE_EVENT,
                        ..press
                    };
                    self.conn
                        .send_event(true, target, EventMask::BUTTON_RELEASE, release)?;
                }
            }
            PointerKind::Up => {
                let release = ButtonReleaseEvent {
                    response_type: BUTTON_RELEASE_EVENT,
                    detail: 1,
                    sequence: 0,
                    time,
                    root: self.root,
                    event: target,
                    child: x11rb::NONE,
                    root_x: rx,
                    root_y: ry,
                    event_x: ex,
                    event_y: ey,
                    state: KeyButMask::BUTTON1,
                    same_screen: true,
                };
                self.conn
                    .send_event(true, target, EventMask::BUTTON_RELEASE, release)?;
            }
        }
        if let Some(c) = self.captured.get_mut(&key) {
            c.dirty = true;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn pointer_xtest(
        &mut self,
        key: u64,
        top: Window,
        kind: PointerKind,
        x: i32,
        y: i32,
    ) -> Result<()> {
        let (rx, ry) = (x as i16, y as i16);
        match kind {
            PointerKind::Down => {
                self.conn.configure_window(
                    top,
                    &ConfigureWindowAux::new()
                        .x(0)
                        .y(0)
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    0,
                    x11rb::CURRENT_TIME,
                    self.root,
                    rx,
                    ry,
                    0,
                )?;
                self.conn.xtest_fake_input(
                    BUTTON_PRESS_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?;
            }
            PointerKind::Move => {
                self.conn.xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    0,
                    x11rb::CURRENT_TIME,
                    self.root,
                    rx,
                    ry,
                    0,
                )?;
            }
            PointerKind::Up => {
                self.conn.xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    0,
                    x11rb::CURRENT_TIME,
                    self.root,
                    rx,
                    ry,
                    0,
                )?;
                self.conn.xtest_fake_input(
                    BUTTON_RELEASE_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?;
                self.conn
                    .configure_window(top, &ConfigureWindowAux::new().x(PARK_X).y(0))?;
            }
            PointerKind::ScrollUp | PointerKind::ScrollDown => {
                let button = if kind == PointerKind::ScrollUp { 4 } else { 5 };
                self.conn.configure_window(
                    top,
                    &ConfigureWindowAux::new()
                        .x(0)
                        .y(0)
                        .stack_mode(StackMode::ABOVE),
                )?;
                self.conn.xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    0,
                    x11rb::CURRENT_TIME,
                    self.root,
                    rx,
                    ry,
                    0,
                )?;
                self.conn.xtest_fake_input(
                    BUTTON_PRESS_EVENT,
                    button,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?;
                self.conn.xtest_fake_input(
                    BUTTON_RELEASE_EVENT,
                    button,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?;
                self.conn
                    .configure_window(top, &ConfigureWindowAux::new().x(PARK_X).y(0))?;
            }
        }
        if let Some(c) = self.captured.get_mut(&key) {
            c.dirty = true;
        }
        self.conn.flush()?;
        Ok(())
    }
}
