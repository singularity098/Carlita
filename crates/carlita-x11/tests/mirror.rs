//! The mirror copies a window on a hidden display and plays taps on it.
//! Needs Xvfb; skipped without it.

use carlita_x11::mirror::{Mirror, MirrorCommand, MirrorEvent, PointerAction};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};
use x11rb::protocol::Event;

struct Xvfb(std::process::Child, u32);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_xvfb() -> Option<Xvfb> {
    let n = (450..600).find(|n| !std::path::Path::new(&format!("/tmp/.X{n}-lock")).exists())?;
    let child = Command::new("Xvfb")
        .args([
            &format!(":{n}"),
            "-screen",
            "0",
            "800x600x24",
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let x = Xvfb(child, n);
    let t = Instant::now();
    while !std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() {
        if t.elapsed() > Duration::from_secs(5) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(x)
}

#[test]
fn copies_the_hidden_editor_and_passes_clicks_on() {
    let Some(xvfb) = start_xvfb() else {
        eprintln!("Xvfb not available; skipping");
        return;
    };
    let (conn, screen) = x11rb::connect(Some(&format!(":{}", xvfb.1))).unwrap();
    let root = conn.setup().roots[screen].root;
    // A plain red "editor", not at the corner (the mirror moves it there).
    let w = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        w,
        root,
        100,
        50,
        120,
        80,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .background_pixel(0x00ff_0000)
            .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
    )
    .unwrap();
    conn.map_window(w).unwrap();
    conn.flush().unwrap();

    let mirror = Mirror::spawn();
    mirror.send(MirrorCommand::Show {
        key: 7,
        display: xvfb.1,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let (width, height, rgba) = loop {
        assert!(Instant::now() < deadline, "no frame");
        match mirror.events().recv_timeout(Duration::from_millis(200)) {
            Ok(MirrorEvent::Frame {
                key: 7,
                width,
                height,
                rgba,
            }) => break (width, height, rgba),
            Ok(MirrorEvent::NotFound { .. }) => panic!("editor not found"),
            _ => {}
        }
    };
    assert_eq!((width, height), (120, 80));
    assert_eq!(&rgba[..4], &[0xff, 0, 0, 0xff]);

    mirror.send(MirrorCommand::Pointer {
        key: 7,
        action: PointerAction::Press(1),
        x: 10,
        y: 20,
    });
    mirror.send(MirrorCommand::Pointer {
        key: 7,
        action: PointerAction::Release(1),
        x: 10,
        y: 20,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no click arrived");
        match conn.poll_for_event().unwrap() {
            Some(Event::ButtonPress(b)) => {
                assert_eq!(b.event, w);
                assert_eq!((b.event_x, b.event_y, b.detail), (10, 20, 1));
                break;
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    // Closing the editor is reported.
    conn.destroy_window(w).unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "loss not reported");
        if let Ok(MirrorEvent::Lost { key: 7 }) =
            mirror.events().recv_timeout(Duration::from_millis(200))
        {
            break;
        }
    }
}
