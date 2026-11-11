//! Carlita: a touch-first plugin chain host for a pedalboard.

mod app;
mod midi_in;
mod scan;
mod system;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
                     SLINT_BACKEND       e.g. winit-software when no OpenGL is available"
                );
                return Ok(());
            }
            other => log::warn!("ignoring unknown argument {other}"),
        }
    }

    let ui = MainWindow::new()?;
    let app = app::App::new(&ui, opts)?;
    ui.run()?;
    app.borrow_mut().shutdown();
    Ok(())
}
