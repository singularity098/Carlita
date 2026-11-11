//! System integration for the options sidebar, all over the system D-Bus:
//!
//! * [`wifi`] – NetworkManager: radio toggle, scan, connect.
//! * [`bluetooth`] – BlueZ: power toggle, discovery, pair and connect.
//! * [`power`] – logind: shut down, reboot and backlight brightness.
//!
//! All functions are blocking; call them from a worker thread.

pub mod bluetooth;
pub mod power;
pub mod wifi;

use zbus::zvariant::OwnedValue;

pub(crate) fn system_bus() -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::Connection::system()
}

pub(crate) fn prop<T>(props: &std::collections::HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}
