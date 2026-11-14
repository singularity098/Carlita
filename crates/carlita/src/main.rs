//! Carlita: a touch-first plugin chain host for a pedalboard.

mod app;
mod icons;
mod meters;
mod midi_in;
mod recorder;
mod scan;
mod system;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    restore_host_library_path();

    let mut opts = app::StartOptions::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--windowed" => opts.fullscreen = Some(false),
            "--fullscreen" | "--kiosk" => opts.fullscreen = Some(true),
            "--no-preset" => opts.skip_default_preset = true,
            "--version" => {
                println!("carlita {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Usage: carlita [--windowed|--fullscreen] [--no-preset]\n\n\
                     Environment:\n  CARLITA_HOME        keep all settings/presets/cache below this folder\n  \
                     CARLA_LIB_DIR       folder with libcarla_standalone2 and Carla's bridges\n  \
                     CARLITA_WAYLAND=1   open as a Wayland window (editors then open in their own windows)\n  \
                     SLINT_BACKEND       e.g. winit-software when no OpenGL is available"
                );
                return Ok(());
            }
            other => log::warn!("ignoring unknown argument {other}"),
        }
    }

    use_x11()?;
    let ui = MainWindow::new()?;
    icons::install(&ui);
    let app = app::App::new(&ui, opts)?;
    ui.run()?;
    app.borrow_mut().shutdown();
    Ok(())
}

/// Opens the main window as an X11 window (through XWayland on a Wayland
/// desktop). Plugin editors are X11 windows and are shown inside Carlita's
/// window by reparenting them into it, which needs an X11 parent; as a
/// Wayland window Carlita could only leave them as windows of their own.
/// `CARLITA_WAYLAND=1` keeps a native Wayland window, and `SLINT_BACKEND`
/// naming another backend is left alone.
fn use_x11() -> anyhow::Result<()> {
    use slint::winit_030::winit::{event_loop::EventLoop, platform::x11::EventLoopBuilderExtX11};
    use slint::winit_030::SlintEvent;

    if std::env::var_os("DISPLAY").is_none() || std::env::var_os("CARLITA_WAYLAND").is_some() {
        return Ok(());
    }
    if std::env::var("SLINT_BACKEND").is_ok_and(|b| !b.to_lowercase().starts_with("winit")) {
        return Ok(());
    }
    let mut builder = EventLoop::<SlintEvent>::with_user_event();
    builder.with_x11();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(builder)
        .select()?;
    log::info!("using X11 for the main window, so plugin editors can be shown inside it");
    Ok(())
}

/// In the AppImage, `AppRun` points `LD_LIBRARY_PATH` at the bundled
/// libraries so Carlita starts, and keeps the desktop's own value in
/// `CARLITA_HOST_LD_LIBRARY_PATH`. The loader has read it by now, so put the
/// desktop's value back for the programs Carlita starts: the plugin scanner
/// (which finds the bundled libraries through its RPATH), and through it
/// yabridge and Wine, which failed with the bundled libraries in their way;
/// also nmcli, bluetoothctl and the like.
fn restore_host_library_path() {
    let Some(host) = std::env::var_os("CARLITA_HOST_LD_LIBRARY_PATH") else {
        return;
    };
    // Single-threaded still: nothing else reads the environment yet.
    if host.is_empty() {
        std::env::remove_var("LD_LIBRARY_PATH");
    } else {
        std::env::set_var("LD_LIBRARY_PATH", host);
    }
    std::env::remove_var("CARLITA_HOST_LD_LIBRARY_PATH");
}
