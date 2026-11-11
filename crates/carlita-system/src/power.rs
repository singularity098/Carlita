//! Shut down / reboot and backlight brightness through systemd-logind.
//!
//! logind lets the user of the active session do both without root.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use zbus::blocking::Proxy;

const LOGIND: &str = "org.freedesktop.login1";

fn manager(conn: &zbus::blocking::Connection) -> Result<Proxy<'_>> {
    Ok(Proxy::new(
        conn,
        LOGIND,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?)
}

pub fn power_off() -> Result<()> {
    let conn = crate::system_bus()?;
    manager(&conn)?.call_method("PowerOff", &(false,))?;
    Ok(())
}

pub fn reboot() -> Result<()> {
    let conn = crate::system_bus()?;
    manager(&conn)?.call_method("Reboot", &(false,))?;
    Ok(())
}

/// A backlight device under /sys/class/backlight.
#[derive(Debug, Clone)]
pub struct Backlight {
    pub name: String,
    path: PathBuf,
    max: u32,
}

impl Backlight {
    /// The first backlight device, if the hardware has one.
    pub fn find() -> Option<Self> {
        Self::find_in(Path::new("/sys/class/backlight"))
    }

    fn find_in(dir: &Path) -> Option<Self> {
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        entries.into_iter().find_map(|e| {
            let path = e.path();
            let max = std::fs::read_to_string(path.join("max_brightness"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            Some(Self {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                max,
            })
        })
    }

    /// Current brightness in percent.
    pub fn percent(&self) -> Result<u32> {
        let raw: u32 = std::fs::read_to_string(self.path.join("brightness"))?
            .trim()
            .parse()?;
        Ok((raw * 100 + self.max / 2).checked_div(self.max).unwrap_or(0))
    }

    /// Sets brightness in percent (clamped to 5..=100 so the screen never goes black).
    pub fn set_percent(&self, percent: u32) -> Result<()> {
        if self.max == 0 {
            bail!("backlight reports no range");
        }
        let raw = self.max * percent.clamp(5, 100) / 100;
        // Direct write works if udev grants access; otherwise ask logind.
        if std::fs::write(self.path.join("brightness"), raw.to_string()).is_ok() {
            return Ok(());
        }
        let conn = crate::system_bus()?;
        let session = Proxy::new(
            &conn,
            LOGIND,
            "/org/freedesktop/login1/session/auto",
            "org.freedesktop.login1.Session",
        )?;
        session
            .call_method("SetBrightness", &("backlight", self.name.as_str(), raw))
            .context("logind refused to set the brightness")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fake_backlight() {
        let dir = std::env::temp_dir().join(format!("carlita-bl-{}", std::process::id()));
        let dev = dir.join("intel_backlight");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("max_brightness"), "1000\n").unwrap();
        std::fs::write(dev.join("brightness"), "500\n").unwrap();
        let bl = Backlight::find_in(&dir).unwrap();
        assert_eq!(bl.name, "intel_backlight");
        assert_eq!(bl.percent().unwrap(), 50);
        bl.set_percent(80).unwrap();
        assert_eq!(bl.percent().unwrap(), 80);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
