//! Creates a fake main window and fake "plugin editor" windows, then checks
//! that the editor host finds an editor by title, moves it into the main
//! window's area, cuts it down to the part an overlay leaves uncovered (with
//! a snapshot of it for the covered part), hides and shows it, and hands it
//! back.
//! Needs an X server (CI uses Xvfb); skipped when DISPLAY is not set.

use carlita_x11::*;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
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

/// The tests share the X server, and one takes any new window as an
/// editor: run them one at a time.
static X_SERVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _x = X_SERVER.lock().unwrap_or_else(|e| e.into_inner());
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
        uncovered: area,
    });
    host.send(HostCommand::Attach {
        key: 42,
        title: "carlita-editor-42".into(),
        parent: main,
        resizable: false,
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

    let geometry = |w| {
        let g = conn.get_geometry(w).unwrap().reply().unwrap();
        (g.x, g.y, g.width, g.height)
    };

    // A sidebar slides over the left 350 px: the holder keeps the part to
    // its right, and the editor stays where it was inside it. A snapshot of
    // the whole editor (red) comes first.
    let left = Area {
        x: 350,
        y: 0,
        width: 450,
        height: 600,
    };
    host.send(HostCommand::SetArea {
        area,
        uncovered: left,
    });
    let (x, y, w, h, rgba) = wait(&host, |e| match e {
        HostEvent::Snapshot {
            key: 42,
            x,
            y,
            width,
            height,
            rgba,
        } => Some((x, y, width, height, rgba)),
        _ => None,
    });
    assert_eq!((x, y, w, h), (300, 250, 200, 100));
    assert_eq!(rgba.len(), 200 * 100 * 4);
    assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
    until(|| geometry(holder) == (350, 250, 150, 100));
    until(|| geometry(editor) == (-50, 0, 200, 100));
    assert!(viewable(editor));

    // Covered entirely: hidden, but still embedded.
    host.send(HostCommand::SetArea {
        area,
        uncovered: Area::default(),
    });
    until(|| !viewable(holder));
    host.send(HostCommand::SetArea {
        area,
        uncovered: area,
    });
    until(|| viewable(editor));
    until(|| geometry(holder) == (300, 250, 200, 100));
    until(|| geometry(editor) == (0, 0, 200, 100));

    // The editor moves itself inside the holder (toolkits do when they
    // resize, to where they were as a window of their own): it is put back.
    conn.configure_window(editor, &ConfigureWindowAux::new().x(400).y(300))
        .unwrap();
    conn.flush().unwrap();
    until(|| geometry(editor) == (0, 0, 200, 100));

    // It changes its own size and moves at once (a plugin's size menu): it
    // is centred again at its new size.
    conn.configure_window(
        editor,
        &ConfigureWindowAux::new()
            .x(500)
            .y(10)
            .width(300)
            .height(160),
    )
    .unwrap();
    conn.flush().unwrap();
    until(|| geometry(holder) == (20 + 230, 50 + 170, 300, 160));
    until(|| geometry(editor) == (0, 0, 300, 160));

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
        resizable: false,
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

    // A JUCE editor's window claims one fixed size (minimum = maximum =
    // its size), but the plugin says it can be resized: it fills the area.
    let editor3 = window(&conn, root, 300, 200, b"carlita-editor-44");
    let mut fixed = WmSizeHints::new();
    fixed.min_size = Some((300, 200));
    fixed.max_size = Some((300, 200));
    fixed.set_normal_hints(&conn, editor3).unwrap();
    conn.map_window(editor3).unwrap();
    conn.flush().unwrap();
    host.send(HostCommand::Attach {
        key: 44,
        title: "carlita-editor-44".into(),
        parent: main,
        resizable: true,
    });
    wait(&host, |e| {
        matches!(e, HostEvent::Attached { key: 44, .. }).then_some(())
    });
    until(|| {
        let g = conn.get_geometry(editor3).unwrap().reply().unwrap();
        (g.width, g.height) == (760, 500)
    });
    host.send(HostCommand::Detach { key: 44 });
    until(|| parent_of(&conn, editor3) == root);
}

/// An editor that does not use the title Carla gives it is still found: it
/// is the one new window of this process that appears after the request.
#[test]
fn finds_an_editor_with_its_own_title() {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("no DISPLAY; skipping");
        return;
    }
    let _x = X_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    let main = window(&conn, root, 800, 600, b"carlita-test-main-2");
    conn.map_window(main).unwrap();
    // Shown before the request: never taken.
    let other = window(&conn, root, 120, 80, b"some other program");
    conn.map_window(other).unwrap();
    conn.flush().unwrap();

    let host = EditorHost::spawn();
    let area = Area {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    };
    host.send(HostCommand::SetArea {
        area,
        uncovered: area,
    });
    host.send(HostCommand::Attach {
        key: 7,
        title: "carlita-editor-7".into(),
        parent: main,
        resizable: false,
    });
    std::thread::sleep(Duration::from_millis(200));
    let editor = window(&conn, root, 240, 160, b"Xythos");
    conn.map_window(editor).unwrap();
    conn.flush().unwrap();

    wait(&host, |e| match e {
        HostEvent::Attached { key: 7, .. } => Some(()),
        HostEvent::NotFound { .. } => panic!("window not found"),
        _ => None,
    });
    let holder = parent_of(&conn, editor);
    assert_eq!(parent_of(&conn, holder), main);
    assert_eq!(parent_of(&conn, other), root);
    host.send(HostCommand::Detach { key: 7 });
    until(|| parent_of(&conn, editor) == root);
}

/// The edge windows pass clicks over an editor on to the main window, F11
/// is reported even while the editor has the keyboard focus, and the focus
/// goes back to the main window when the pointer leaves the editor.
#[test]
fn edges_f11_and_focus() {
    use x11rb::protocol::xtest::ConnectionExt as _;
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("no DISPLAY; skipping");
        return;
    }
    let _x = X_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let (conn, screen) = x11rb::connect(None).unwrap();
    let root = conn.setup().roots[screen].root;
    if conn
        .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
        .unwrap()
        .is_none()
    {
        eprintln!("no XTEST; skipping");
        return;
    }

    let main = window(&conn, root, 800, 600, b"carlita-test-main-2");
    conn.change_window_attributes(
        main,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
    )
    .unwrap();
    conn.map_window(main).unwrap();
    let editor = window(&conn, root, 200, 100, b"carlita-editor-7");
    let mut hints = WmSizeHints::new();
    hints.min_size = Some((200, 100));
    hints.max_size = Some((200, 100));
    hints.set_normal_hints(&conn, editor).unwrap();
    conn.change_window_attributes(
        editor,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
    )
    .unwrap();
    conn.map_window(editor).unwrap();
    conn.flush().unwrap();

    let host = EditorHost::spawn();
    let area = Area {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
    };
    host.send(HostCommand::SetArea {
        area,
        uncovered: area,
    });
    host.send(HostCommand::Attach {
        key: 7,
        title: "carlita-editor-7".into(),
        parent: main,
        resizable: false,
    });
    wait(&host, |e| match e {
        HostEvent::Attached { .. } => Some(()),
        HostEvent::NotFound { .. } => panic!("window not found"),
        _ => None,
    });
    // The editor is centred: x 300..500, y 250..350. An edge strip covers
    // the left half of the window, and so the editor's left half.
    host.send(HostCommand::SetEdges {
        parent: main,
        edges: vec![Area {
            x: 0,
            y: 0,
            width: 400,
            height: 600,
        }],
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
    until(|| {
        let children = conn.query_tree(main).unwrap().reply().unwrap().children;
        children.len() == 2 && viewable(*children.last().unwrap())
    });
    let main_at = conn
        .translate_coordinates(main, root, 0, 0)
        .unwrap()
        .reply()
        .unwrap();
    let (ox, oy) = (main_at.dst_x, main_at.dst_y);

    // Drains this connection's events and returns the windows clicked.
    let clicked = |conn: &RustConnection| {
        conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let mut out = Vec::new();
        while let Some(ev) = conn.poll_for_event().unwrap() {
            if let x11rb::protocol::Event::ButtonPress(b) = ev {
                out.push(b.event);
            }
        }
        out
    };
    let click = |x: i16, y: i16| {
        conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, root, ox + x, oy + y, 0)
            .unwrap();
        conn.xtest_fake_input(BUTTON_PRESS_EVENT, 1, 0, root, 0, 0, 0)
            .unwrap();
        conn.xtest_fake_input(BUTTON_RELEASE_EVENT, 1, 0, root, 0, 0, 0)
            .unwrap();
    };
    clicked(&conn);
    // Over the editor and the strip: the main window gets the click.
    click(350, 300);
    assert_eq!(clicked(&conn), vec![main]);
    // Over the editor alone: the editor does.
    click(450, 300);
    assert_eq!(clicked(&conn), vec![editor]);

    // F11 while the editor has the keyboard focus.
    conn.set_input_focus(InputFocus::PARENT, editor, x11rb::CURRENT_TIME)
        .unwrap();
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn
        .get_keyboard_mapping(min, max - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let per = map.keysyms_per_keycode as usize;
    let f11 = min
        + map
            .keysyms
            .chunks(per)
            .position(|s| s.contains(&0xffc8))
            .unwrap() as u8;
    conn.xtest_fake_input(KEY_PRESS_EVENT, f11, 0, root, 0, 0, 0)
        .unwrap();
    conn.xtest_fake_input(KEY_RELEASE_EVENT, f11, 0, root, 0, 0, 0)
        .unwrap();
    conn.flush().unwrap();
    wait(&host, |e| {
        matches!(e, HostEvent::ToggleFullscreen).then_some(())
    });

    // The pointer leaves the editor: the main window gets the focus back.
    let focus = |conn: &RustConnection| conn.get_input_focus().unwrap().reply().unwrap().focus;
    assert_eq!(focus(&conn), editor);
    conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, root, ox + 700, oy + 50, 0)
        .unwrap();
    conn.flush().unwrap();
    until(|| focus(&conn) == main);

    drop(host);
}
