//! Creates a fake "plugin editor" window, then checks that the editor host
//! finds it by title, captures its pixels off-screen and forwards a click.
//! Needs an X server with Composite and Damage (CI uses Xvfb); skipped when
//! DISPLAY is not set.

use carlita_x11::*;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

#[test]
fn captures_and_forwards_input() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("no DISPLAY; skipping");
        return;
    }
    let (conn, screen) = x11rb::connect(None).unwrap();
    let s = &conn.setup().roots[screen];
    let win = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        s.root,
        10,
        10,
        200,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(0x00FF0000)
            .event_mask(EventMask::BUTTON_PRESS | EventMask::EXPOSURE),
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"carlita-editor-42",
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();

    let host = EditorHost::spawn();
    host.send(HostCommand::Attach {
        key: 42,
        title: "carlita-editor-42".into(),
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut attached = false;
    let mut frame_ok = false;
    while Instant::now() < deadline && !(attached && frame_ok) {
        match host.events().recv_timeout(Duration::from_millis(100)) {
            Ok(HostEvent::Attached {
                key, width, height, ..
            }) => {
                assert_eq!((key, width, height), (42, 200, 100));
                attached = true;
            }
            Ok(HostEvent::Frame(f)) => {
                assert_eq!((f.width, f.height), (200, 100));
                let mid = ((50 * 200 + 100) * 4) as usize;
                // Background is pure red.
                frame_ok = f.rgba[mid..mid + 3] == [255, 0, 0];
            }
            Ok(HostEvent::Unavailable(e)) => panic!("capture unavailable: {e}"),
            Ok(HostEvent::NotFound { .. }) => panic!("window not found"),
            _ => {}
        }
    }
    assert!(
        attached && frame_ok,
        "attached={attached} frame_ok={frame_ok}"
    );

    // The window was parked off-screen.
    let geo = conn.get_geometry(win).unwrap().reply().unwrap();
    assert!(geo.x < -1000, "window not parked: x={}", geo.x);

    // A forwarded click arrives at the window in editor coordinates.
    host.send(HostCommand::Pointer {
        key: 42,
        kind: PointerKind::Down,
        x: 30,
        y: 40,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut clicked = None;
    while Instant::now() < deadline && clicked.is_none() {
        if let Ok(Some(Event::ButtonPress(e))) = conn.poll_for_event() {
            clicked = Some((e.event_x, e.event_y, e.detail));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(clicked, Some((30, 40, 1)));

    host.send(HostCommand::Detach { key: 42 });
}
