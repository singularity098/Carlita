//! Creates a fake main window and fake "plugin editor" windows, then checks
//! that the editor host finds an editor by title, moves it into the main
//! window's area, hides and shows it, and hands it back.
//! Needs an X server (CI uses Xvfb); skipped when DISPLAY is not set.

use carlita_x11::*;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn window(conn: &RustConnection, parent: Window, w: u16, h: u16, title: &[u8]) -> Window {
    let win = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        parent,
        10,
        10,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(0x00FF0000),
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title,
    )
    .unwrap();
    win
}

fn parent_of(conn: &RustConnection, w: Window) -> Window {
    conn.query_tree(w).unwrap().reply().unwrap().parent
}

fn wait<T>(host: &EditorHost, mut f: impl FnMut(HostEvent) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match host.events().recv_timeout(Duration::from_millis(100)) {
            Ok(HostEvent::Unavailable(e)) => panic!("embedding unavailable: {e}"),
            Ok(e) => {
                if let Some(t) = f(e) {
                    return t;
                }
            }
            Err(_) => {}
        }
    }
    panic!("timed out");
}

/// Polls until `f` holds (the host works on its own connection).
fn until(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn embeds_hides_and_hands_back() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("no DISPLAY; skipping");
        return;
    }
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;

    let main = window(&conn, root, 800, 600, b"carlita-test-main");
    conn.map_window(main).unwrap();

    // A fixed-size editor.
    let editor = window(&conn, root, 200, 100, b"carlita-editor-42");
    let mut hints = WmSizeHints::new();
    hints.min_size = Some((200, 100));
    hints.max_size = Some((200, 100));
    hints.set_normal_hints(&conn, editor).unwrap();
    conn.map_window(editor).unwrap();
    conn.flush().unwrap();

    let host = EditorHost::spawn();
    let area = Area {
        x: 20,
        y: 50,
        width: 760,
        height: 500,
    };
    host.send(HostCommand::SetArea {
        area,
        visible: true,
    });
    host.send(HostCommand::Attach {
        key: 42,
        title: "carlita-editor-42".into(),
        parent: main,
    });
    let size = wait(&host, |e| match e {
        HostEvent::Attached { key, width, height } => Some((key, width, height)),
        HostEvent::NotFound { .. } => panic!("window not found"),
        _ => None,
    });
    assert_eq!(size, (42, 200, 100));

    // The editor sits in a holder inside the main window, centred in the
    // area and shown.
    let holder = parent_of(&conn, editor);
    assert_ne!(holder, root);
    assert_eq!(parent_of(&conn, holder), main);
    until(|| {
        let g = conn.get_geometry(holder).unwrap().reply().unwrap();
        (g.x, g.y, g.width, g.height) == (20 + 280, 50 + 200, 200, 100)
    });
    let viewable = |w| {
        conn.get_window_attributes(w)
            .unwrap()
            .reply()
            .unwrap()
            .map_state
            == MapState::VIEWABLE
    };
    until(|| viewable(editor));

    // Covered: hidden, but still embedded.
    host.send(HostCommand::SetArea {
        area,
        visible: false,
    });
    until(|| !viewable(holder));
    host.send(HostCommand::SetArea {
        area,
        visible: true,
    });
    until(|| viewable(editor));

    // Detached: back to an unmapped top-level window.
    host.send(HostCommand::Detach { key: 42 });
    until(|| parent_of(&conn, editor) == root);
    assert!(!viewable(editor));

    // A resizable editor is given the whole area, and losing it is reported.
    let editor2 = window(&conn, root, 300, 200, b"carlita-editor-43");
    conn.map_window(editor2).unwrap();
    conn.flush().unwrap();
    host.send(HostCommand::Attach {
        key: 43,
        title: "carlita-editor-43".into(),
        parent: main,
    });
    wait(&host, |e| {
        matches!(e, HostEvent::Attached { key: 43, .. }).then_some(())
    });
    until(|| {
        let g = conn.get_geometry(editor2).unwrap().reply().unwrap();
        (g.width, g.height) == (760, 500)
    });
    conn.destroy_window(editor2).unwrap();
    conn.flush().unwrap();
    wait(&host, |e| {
        matches!(e, HostEvent::Lost { key: 43 }).then_some(())
    });
}
