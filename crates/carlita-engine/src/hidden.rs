//! Hidden X servers (Xvfb) for plugins whose editors are drawn off screen.
//!
//! Very old plugins draw their editors at a fixed, small size and cannot be
//! scaled. Such a plugin can run on a hidden display of its own: Carlita
//! starts an Xvfb server, and the plugin's bridge process is started with
//! `DISPLAY` pointing at it, so its editor can never appear on the real
//! screen. Carlita's editor area then shows a magnified copy of it and
//! passes taps and the mouse on (`carlita-x11`'s mirror).
//!
//! Carla starts a bridge from `<binary dir>/carla-bridge-native`, a path it
//! takes from its options when the plugin is added. For a hidden plugin the
//! engine points that option at a folder of its own for the moment it adds
//! the plugin: the folder links to every file of Carla's, except that
//! `carla-bridge-native` is a small script that sets `DISPLAY` and starts
//! the real one.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The hidden screen's size: room for the largest old editors.
const SCREEN: &str = "2560x1600x24";
/// How long to wait for a new Xvfb to take connections.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// One running Xvfb server and the bridge folder that uses it. The server
/// is stopped when this is dropped.
pub struct HiddenDisplay {
    pub number: u32,
    child: Child,
    folder: PathBuf,
}

/// Whether hidden displays can be made (Xvfb is installed).
pub fn available() -> bool {
    find_xvfb().is_some()
}

fn find_xvfb() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("Xvfb"))
        .find(|p| p.is_file())
}

fn socket(n: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{n}"))
}

fn in_use(n: u32) -> bool {
    socket(n).exists() || Path::new(&format!("/tmp/.X{n}-lock")).exists()
}

impl HiddenDisplay {
    /// Starts an Xvfb on a free display number and makes the bridge folder
    /// for it, linking to Carla's files in `binaries`.
    pub fn start(binaries: &Path) -> Result<Self> {
        let xvfb = find_xvfb().context("Xvfb is not installed (package xvfb)")?;
        let number = (90..400)
            .find(|&n| !in_use(n))
            .context("no free X display number")?;
        let child = Command::new(xvfb)
            .arg(format!(":{number}"))
            .args(["-screen", "0", SCREEN, "-nolisten", "tcp", "-noreset"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot start Xvfb")?;
        let mut display = HiddenDisplay {
            number,
            child,
            folder: PathBuf::new(),
        };
        let started = Instant::now();
        while !socket(number).exists() {
            if let Ok(Some(status)) = display.child.try_wait() {
                bail!("Xvfb on :{number} stopped at once ({status})");
            }
            if started.elapsed() > START_TIMEOUT {
                bail!("Xvfb on :{number} did not start");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        display.folder = make_folder(binaries, number)?;
        log::info!("hidden display :{number} started");
        Ok(display)
    }

    /// The folder to give Carla as its binary folder while adding the plugin.
    pub fn folder(&self) -> &Path {
        &self.folder
    }
}

impl Drop for HiddenDisplay {
    fn drop(&mut self) {
        // SIGTERM, so Xvfb removes its socket and lock file.
        let _ = Command::new("kill")
            .arg(self.child.id().to_string())
            .stderr(Stdio::null())
            .status();
        let started = Instant::now();
        while matches!(self.child.try_wait(), Ok(None)) {
            if started.elapsed() > Duration::from_secs(2) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !self.folder.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.folder);
        }
        log::info!("hidden display :{} stopped", self.number);
    }
}

/// The bridge folder for display `number`.
fn make_folder(binaries: &Path, number: u32) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let folder = base.join(format!("carlita-{}-hidden-{number}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder)?;
    for entry in std::fs::read_dir(binaries)
        .with_context(|| format!("cannot read {}", binaries.display()))?
    {
        let entry = entry?;
        if entry.file_name() == "carla-bridge-native" {
            continue;
        }
        let _ = std::os::unix::fs::symlink(entry.path(), folder.join(entry.file_name()));
    }
    let real = binaries.join("carla-bridge-native");
    let script = folder.join("carla-bridge-native");
    std::fs::write(&script, bridge_script(&real, number))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok(folder)
}

/// The script that starts the real bridge on the hidden display. Editors
/// there are drawn at their own size (Carlita magnifies the copy it shows),
/// and toolkits are kept off Wayland and off the desktop's scaling.
fn bridge_script(real: &Path, number: u32) -> String {
    let quoted = real.to_string_lossy().replace('\'', r"'\''");
    format!(
        "#!/bin/sh\n\
         # Made by Carlita: runs a plugin bridge on the hidden display :{number}.\n\
         export DISPLAY=:{number}\n\
         unset WAYLAND_DISPLAY GDK_SCALE GDK_DPI_SCALE QT_SCALE_FACTOR QT_SCREEN_SCALE_FACTORS\n\
         export CARLA_BRIDGE_UI_SCALE=1.000\n\
         exec '{quoted}' \"$@\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sets_the_display_and_quotes_the_path() {
        let s = bridge_script(Path::new("/opt/it's/carla-bridge-native"), 93);
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("export DISPLAY=:93\n"));
        assert!(s.contains(r"exec '/opt/it'\''s/carla-bridge-native' "));
    }

    #[test]
    fn folder_links_carla_and_replaces_the_bridge() {
        let bin = tempfile::tempdir().unwrap();
        std::fs::write(bin.path().join("carla-bridge-native"), "").unwrap();
        std::fs::write(bin.path().join("carla-discovery-native"), "").unwrap();
        let f = make_folder(bin.path(), 391).unwrap();
        assert!(f
            .join("carla-discovery-native")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        let script = std::fs::read_to_string(f.join("carla-bridge-native")).unwrap();
        assert!(script.contains("DISPLAY=:391"));
        std::fs::remove_dir_all(f).unwrap();
    }

    #[test]
    fn starts_xvfb_and_bridges_run_on_it() {
        if !available() {
            eprintln!("Xvfb not installed; skipping");
            return;
        }
        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("carla-bridge-native");
        std::fs::write(&fake, "#!/bin/sh\necho \"$DISPLAY $1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = HiddenDisplay::start(bin.path()).unwrap();
        let n = d.number;
        assert!(socket(n).exists());
        let out = Command::new(d.folder().join("carla-bridge-native"))
            .arg("VST3")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!(":{n} VST3\n"));
        let folder = d.folder().to_path_buf();
        drop(d);
        assert!(!socket(n).exists(), "Xvfb left its socket behind");
        assert!(!folder.exists());
    }
}
