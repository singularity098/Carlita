//! Shows an editor that runs on a hidden X display (Xvfb).
//!
//! Old plugins whose editors cannot be scaled can run on a hidden display
//! of their own (see `carlita_engine::hidden`). Their editor window is
//! drawn there, never on the real screen. This thread copies what the
//! editor shows, a few dozen times a second, and sends the pictures to the
//! UI, which draws them magnified in the editor area; taps and the mouse
//! there come back here and are played on the hidden display with XTest.
//!
//! The hidden display has nothing else on it and no window manager, so the
//! editor is simply the largest window shown there.

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, ImageFormat, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// How often the editor is copied.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// How long to look for the editor's window (old Windows plugins start
/// slowly under Wine).
const FIND_TIMEOUT: Duration = Duration::from_secs(120);
/// How often to look for it.
const SEARCH_INTERVAL: Duration = Duration::from_millis(150);

/// What the pointer did, at a point in the editor (its own pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAction {
    Move,
    /// X button number: 1 left, 2 middle, 3 right.
    Press(u8),
    Release(u8),
    /// Wheel steps: positive is up (or right).
    Scroll {
        dx: i32,
        dy: i32,
    },
}

pub enum MirrorCommand {
    /// Show the editor on display `:display`. Send it before the editor is
    /// opened. Replaces whatever was shown.
    Show {
        key: u64,
        display: u32,
    },
    /// Stop showing it.
    Hide {
        key: u64,
    },
    Pointer {
        key: u64,
        action: PointerAction,
        x: i32,
        y: i32,
    },
    Shutdown,
}

pub enum MirrorEvent {
    /// What the editor shows now: `width * height` RGBA pixels.
    Frame {
        key: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    NotFound {
        key: u64,
    },
    /// The editor's window went away (closed or crashed).
    Lost {
        key: u64,
    },
}

/// Handle to the mirror thread.
pub struct Mirror {
    tx: Sender<MirrorCommand>,
    events: Receiver<MirrorEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mirror {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let thread = std::thread::Builder::new()
            .name("carlita-mirror".into())
            .spawn(move || run(rx, ev_tx))
            .expect("spawn mirror thread");
        Self {
            tx,
            events: ev_rx,
            thread: Some(thread),
        }
    }

    pub fn send(&self, c: MirrorCommand) {
        let _ = self.tx.send(c);
    }

    pub fn events(&self) -> &Receiver<MirrorEvent> {
        &self.events
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        let _ = self.tx.send(MirrorCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Shown {
    key: u64,
    conn: RustConnection,
    root: Window,
    window: Option<Window>,
    since: Instant,
    last_search: Instant,
    last: Vec<u8>,
    size: (u32, u32),
}

fn run(rx: Receiver<MirrorCommand>, events: Sender<MirrorEvent>) {
    let mut shown: Option<Shown> = None;
    let mut next_frame = Instant::now();
    loop {
        let wait = next_frame.saturating_duration_since(Instant::now());
        match rx.recv_timeout(if shown.is_some() {
            wait
        } else {
            Duration::from_secs(1)
        }) {
            Ok(MirrorCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(MirrorCommand::Show { key, display }) => {
                shown = match connect(key, display) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        log::warn!("cannot reach hidden display :{display}: {e:#}");
                        let _ = events.send(MirrorEvent::NotFound { key });
                        None
                    }
                };
            }
            Ok(MirrorCommand::Hide { key }) => {
                if shown.as_ref().is_some_and(|s| s.key == key) {
                    shown = None;
                }
            }
            Ok(MirrorCommand::Pointer { key, action, x, y }) => {
                if let Some(s) = shown.as_mut().filter(|s| s.key == key) {
                    s.pointer(action, x, y);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() < next_frame {
            continue;
        }
        next_frame = Instant::now() + FRAME_INTERVAL;
        let Some(s) = shown.as_mut() else { continue };
        let key = s.key;
        match s.tick() {
            Tick::Nothing => {}
            Tick::Frame(width, height, rgba) => {
                let _ = events.send(MirrorEvent::Frame {
                    key,
                    width,
                    height,
                    rgba,
                });
            }
            Tick::NotFound => {
                let _ = events.send(MirrorEvent::NotFound { key });
                shown = None;
            }
            Tick::Lost => {
                let _ = events.send(MirrorEvent::Lost { key });
                shown = None;
            }
        }
    }
}

fn connect(key: u64, display: u32) -> Result<Shown> {
    let (conn, screen) = x11rb::connect(Some(&format!(":{display}")))
        .with_context(|| format!("cannot connect to :{display}"))?;
    let root = conn.setup().roots[screen].root;
    // Fails early if the server has no XTest (Xvfb has).
    conn.xtest_get_version(2, 2)?.reply()?;
    Ok(Shown {
        key,
        conn,
        root,
        window: None,
        since: Instant::now(),
        last_search: Instant::now() - SEARCH_INTERVAL,
        last: Vec::new(),
        size: (0, 0),
    })
}

enum Tick {
    Nothing,
    Frame(u32, u32, Vec<u8>),
    NotFound,
    Lost,
}

impl Shown {
    fn tick(&mut self) -> Tick {
        let Some(w) = self.window else {
            if self.last_search.elapsed() < SEARCH_INTERVAL {
                return Tick::Nothing;
            }
            self.last_search = Instant::now();
            match self.find() {
                Some(w) => {
                    log::info!("found the editor on the hidden display ({w:#x})");
                    // Top left, so all of it is on the hidden screen.
                    let _ = self
                        .conn
                        .configure_window(w, &ConfigureWindowAux::new().x(0).y(0));
                    let _ = self.conn.flush();
                    self.window = Some(w);
                }
                None if self.since.elapsed() > FIND_TIMEOUT => return Tick::NotFound,
                None => {}
            }
            return Tick::Nothing;
        };
        match self.capture(w) {
            Ok(Some((width, height, rgba))) => Tick::Frame(width, height, rgba),
            Ok(None) => Tick::Nothing,
            Err(e) => {
                log::info!("editor on the hidden display went away: {e:#}");
                Tick::Lost
            }
        }
    }

    /// The largest window shown on the hidden display.
    fn find(&self) -> Option<Window> {
        let tree = self.conn.query_tree(self.root).ok()?.reply().ok()?;
        tree.children
            .iter()
            .filter_map(|&w| {
                let a = self.conn.get_window_attributes(w).ok()?.reply().ok()?;
                if a.map_state != MapState::VIEWABLE {
                    return None;
                }
                let g = self.conn.get_geometry(w).ok()?.reply().ok()?;
                (g.width >= 32 && g.height >= 32)
                    .then_some((u32::from(g.width) * u32::from(g.height), w))
            })
            .max_by_key(|(area, _)| *area)
            .map(|(_, w)| w)
    }

    /// A copy of what the window shows, if it changed since the last one.
    fn capture(&mut self, w: Window) -> Result<Option<(u32, u32, Vec<u8>)>> {
        let attrs = self.conn.get_window_attributes(w)?.reply()?;
        if attrs.map_state != MapState::VIEWABLE {
            anyhow::bail!("window no longer shown");
        }
        let g = self.conn.get_geometry(w)?.reply()?;
        let at = self
            .conn
            .translate_coordinates(w, self.root, 0, 0)?
            .reply()?;
        let (width, height) = (u32::from(g.width), u32::from(g.height));
        // Read from the screen (the root), so windows inside the editor
        // (yabridge's, Wine's) are part of the picture.
        let x = at.dst_x.max(0);
        let y = at.dst_y.max(0);
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                x,
                y,
                g.width,
                g.height,
                !0,
            )?
            .reply()?;
        if image.data.len() < (width * height * 4) as usize {
            anyhow::bail!("unexpected image format (depth {})", image.depth);
        }
        if image.data == self.last && self.size == (width, height) {
            return Ok(None);
        }
        let mut rgba = Vec::with_capacity(image.data.len());
        for px in image.data.chunks_exact(4).take((width * height) as usize) {
            // BGRX, little endian.
            rgba.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
        }
        self.last = image.data;
        self.size = (width, height);
        Ok(Some((width, height, rgba)))
    }

    fn pointer(&self, action: PointerAction, x: i32, y: i32) {
        let Some(w) = self.window else { return };
        let Some(at) = self
            .conn
            .translate_coordinates(w, self.root, 0, 0)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return;
        };
        let (rx, ry) = (
            (i32::from(at.dst_x) + x).clamp(0, i16::MAX as i32) as i16,
            (i32::from(at.dst_y) + y).clamp(0, i16::MAX as i32) as i16,
        );
        const PRESS: u8 = 4;
        const RELEASE: u8 = 5;
        const MOTION: u8 = 6;
        let fake = |kind: u8, detail: u8| {
            let _ =
                self.conn
                    .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, rx, ry, 0);
        };
        fake(MOTION, 0);
        match action {
            PointerAction::Move => {}
            PointerAction::Press(b) => fake(PRESS, b),
            PointerAction::Release(b) => fake(RELEASE, b),
            PointerAction::Scroll { dx, dy } => {
                let steps = |n: i32, pos: u8, neg: u8| {
                    let b = if n > 0 { pos } else { neg };
                    for _ in 0..n.unsigned_abs().min(10) {
                        fake(PRESS, b);
                        fake(RELEASE, b);
                    }
                };
                steps(dy, 4, 5);
                steps(dx, 7, 6);
            }
        }
        let _ = self.conn.flush();
    }
}
