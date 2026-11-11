//! WiFi through NetworkManager's D-Bus API.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const DEVICE_TYPE_WIFI: u32 = 2;
/// NM_802_11_AP_FLAGS_PRIVACY
const AP_FLAGS_PRIVACY: u32 = 0x1;

#[derive(Debug, Clone, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength 0..=100.
    pub strength: u8,
    pub secured: bool,
    pub active: bool,
    /// Whether NetworkManager already has a saved profile for it.
    pub known: bool,
    pub(crate) ap_path: String,
}

pub struct Wifi {
    conn: Connection,
}

impl Wifi {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            conn: crate::system_bus().context("connecting to the system D-Bus")?,
        })
    }

    fn proxy(&self, path: &str, iface: &'static str) -> Result<Proxy<'static>> {
        Ok(Proxy::new(&self.conn, NM, path.to_string(), iface)?)
    }

    fn nm(&self) -> Result<Proxy<'static>> {
        self.proxy(NM_PATH, NM)
    }

    pub fn is_available(&self) -> bool {
        self.wifi_device().is_ok()
    }

    pub fn enabled(&self) -> Result<bool> {
        Ok(self.nm()?.get_property("WirelessEnabled")?)
    }

    pub fn set_enabled(&self, on: bool) -> Result<()> {
        self.nm()?.set_property("WirelessEnabled", on)?;
        Ok(())
    }

    fn wifi_device(&self) -> Result<OwnedObjectPath> {
        let devices: Vec<OwnedObjectPath> = self.nm()?.call("GetDevices", &())?;
        for d in devices {
            let p = self.proxy(d.as_str(), "org.freedesktop.NetworkManager.Device")?;
            if p.get_property::<u32>("DeviceType").unwrap_or(0) == DEVICE_TYPE_WIFI {
                return Ok(d);
            }
        }
        bail!("no WiFi device found")
    }

    /// Asks the device to rescan; results show up in [`Wifi::networks`] a few seconds later.
    pub fn request_scan(&self) -> Result<()> {
        let dev = self.wifi_device()?;
        let p = self.proxy(
            dev.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        )?;
        let opts: HashMap<&str, Value> = HashMap::new();
        p.call_method("RequestScan", &(opts,))?;
        Ok(())
    }

    fn known_ssids(&self) -> Vec<(String, OwnedObjectPath)> {
        let mut out = Vec::new();
        let Ok(settings) = self.proxy(
            "/org/freedesktop/NetworkManager/Settings",
            "org.freedesktop.NetworkManager.Settings",
        ) else {
            return out;
        };
        let Ok(conns) = settings.call::<_, _, Vec<OwnedObjectPath>>("ListConnections", &()) else {
            return out;
        };
        for c in conns {
            let Ok(p) = self.proxy(
                c.as_str(),
                "org.freedesktop.NetworkManager.Settings.Connection",
            ) else {
                continue;
            };
            let Ok(s) = p
                .call::<_, _, HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>>>(
                    "GetSettings",
                    &(),
                )
            else {
                continue;
            };
            if let Some(ssid) = s
                .get("802-11-wireless")
                .and_then(|w| crate::prop::<Vec<u8>>(w, "ssid"))
            {
                out.push((String::from_utf8_lossy(&ssid).into_owned(), c));
            }
        }
        out
    }

    /// Visible networks, strongest first, one entry per SSID.
    pub fn networks(&self) -> Result<Vec<WifiNetwork>> {
        let dev = self.wifi_device()?;
        let wl = self.proxy(
            dev.as_str(),
            "org.freedesktop.NetworkManager.Device.Wireless",
        )?;
        let aps: Vec<OwnedObjectPath> = wl.call("GetAllAccessPoints", &())?;
        let active: OwnedObjectPath = wl
            .get_property("ActiveAccessPoint")
            .unwrap_or_else(|_| OwnedObjectPath::try_from("/").unwrap());
        let known = self.known_ssids();

        let mut nets: Vec<WifiNetwork> = Vec::new();
        for ap in aps {
            let p = self.proxy(ap.as_str(), "org.freedesktop.NetworkManager.AccessPoint")?;
            let ssid: Vec<u8> = p.get_property("Ssid").unwrap_or_default();
            if ssid.is_empty() {
                continue; // hidden network
            }
            let ssid = String::from_utf8_lossy(&ssid).into_owned();
            let flags: u32 = p.get_property("Flags").unwrap_or(0);
            let wpa: u32 = p.get_property("WpaFlags").unwrap_or(0);
            let rsn: u32 = p.get_property("RsnFlags").unwrap_or(0);
            let net = WifiNetwork {
                strength: p.get_property("Strength").unwrap_or(0),
                secured: flags & AP_FLAGS_PRIVACY != 0 || wpa != 0 || rsn != 0,
                active: ap == active,
                known: known.iter().any(|(s, _)| *s == ssid),
                ssid,
                ap_path: ap.to_string(),
            };
            match nets.iter_mut().find(|n| n.ssid == net.ssid) {
                Some(existing) if existing.strength < net.strength && !existing.active => {
                    *existing = net
                }
                Some(existing) => existing.active |= net.active,
                None => nets.push(net),
            }
        }
        nets.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.cmp(&a.strength)));
        Ok(nets)
    }

    /// Connects to a network. `password` is only needed for secured networks
    /// without a saved profile.
    pub fn connect_to(&self, net: &WifiNetwork, password: Option<&str>) -> Result<()> {
        let dev = self.wifi_device()?;
        let ap = ObjectPath::try_from(net.ap_path.as_str())?;
        let nm = self.nm()?;
        if let Some((_, conn)) = self.known_ssids().into_iter().find(|(s, _)| *s == net.ssid) {
            if password.is_none() {
                nm.call_method("ActivateConnection", &(conn, &dev, &ap))?;
                return Ok(());
            }
        }
        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
        settings.insert(
            "802-11-wireless",
            HashMap::from([("ssid", Value::from(net.ssid.as_bytes().to_vec()))]),
        );
        settings.insert(
            "connection",
            HashMap::from([
                ("id", Value::from(net.ssid.clone())),
                ("type", Value::from("802-11-wireless")),
            ]),
        );
        if net.secured {
            let Some(pw) = password else {
                bail!("a password is required for {}", net.ssid)
            };
            settings.insert(
                "802-11-wireless-security",
                HashMap::from([
                    ("key-mgmt", Value::from("wpa-psk")),
                    ("psk", Value::from(pw.to_string())),
                ]),
            );
        }
        nm.call_method("AddAndActivateConnection", &(settings, &dev, &ap))?;
        Ok(())
    }

    pub fn disconnect(&self) -> Result<()> {
        let dev = self.wifi_device()?;
        self.proxy(dev.as_str(), "org.freedesktop.NetworkManager.Device")?
            .call_method("Disconnect", &())?;
        Ok(())
    }
}
