//! Bluetooth through BlueZ's D-Bus API.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const BLUEZ: &str = "org.bluez";
const AGENT_PATH: &str = "/org/carlita/bluetooth_agent";

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[derive(Debug, Clone, PartialEq)]
pub struct BtDevice {
    pub path: String,
    pub name: String,
    pub address: String,
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    pub rssi: Option<i16>,
}

/// Pairing agent that accepts every request without user interaction
/// ("NoInputNoOutput"), which suits MIDI controllers and headphones.
struct AutoAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl AutoAgent {
    fn release(&self) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".into()
    }
    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}
    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }
    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}
    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) {}
    fn request_authorization(&self, _device: OwnedObjectPath) {}
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}
    fn cancel(&self) {}
}

pub struct Bluetooth {
    conn: Connection,
    agent_registered: bool,
}

impl Bluetooth {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            conn: crate::system_bus().context("connecting to the system D-Bus")?,
            agent_registered: false,
        })
    }

    fn objects(&self) -> Result<ManagedObjects> {
        let om = Proxy::new(&self.conn, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager")?;
        Ok(om.call("GetManagedObjects", &())?)
    }

    fn adapter_path(&self) -> Result<OwnedObjectPath> {
        let objs = self.objects()?;
        let mut adapters: Vec<_> = objs
            .into_iter()
            .filter(|(_, ifs)| ifs.contains_key("org.bluez.Adapter1"))
            .map(|(p, _)| p)
            .collect();
        adapters.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        adapters
            .into_iter()
            .next()
            .context("no Bluetooth adapter found")
    }

    fn adapter(&self) -> Result<Proxy<'_>> {
        let path = self.adapter_path()?;
        Ok(Proxy::new(&self.conn, BLUEZ, path, "org.bluez.Adapter1")?)
    }

    fn device(&self, path: &str) -> Result<Proxy<'_>> {
        Ok(Proxy::new(
            &self.conn,
            BLUEZ,
            path.to_string(),
            "org.bluez.Device1",
        )?)
    }

    pub fn is_available(&self) -> bool {
        self.adapter_path().is_ok()
    }

    pub fn powered(&self) -> Result<bool> {
        Ok(self.adapter()?.get_property("Powered")?)
    }

    pub fn set_powered(&self, on: bool) -> Result<()> {
        self.adapter()?.set_property("Powered", on)?;
        Ok(())
    }

    pub fn start_discovery(&self) -> Result<()> {
        let a = self.adapter()?;
        if !a.get_property::<bool>("Discovering").unwrap_or(false) {
            a.call_method("StartDiscovery", &())?;
        }
        Ok(())
    }

    pub fn stop_discovery(&self) -> Result<()> {
        let a = self.adapter()?;
        if a.get_property::<bool>("Discovering").unwrap_or(false) {
            a.call_method("StopDiscovery", &())?;
        }
        Ok(())
    }

    /// Paired devices first, then nearby ones by signal strength.
    pub fn devices(&self) -> Result<Vec<BtDevice>> {
        let mut out: Vec<BtDevice> = self
            .objects()?
            .into_iter()
            .filter_map(|(path, ifs)| {
                let d = ifs.get("org.bluez.Device1")?;
                let address: String = crate::prop(d, "Address").unwrap_or_default();
                let name: String = crate::prop(d, "Alias")
                    .or_else(|| crate::prop(d, "Name"))
                    .unwrap_or_else(|| address.clone());
                Some(BtDevice {
                    path: path.to_string(),
                    name,
                    address,
                    icon: crate::prop(d, "Icon").unwrap_or_default(),
                    paired: crate::prop(d, "Paired").unwrap_or(false),
                    connected: crate::prop(d, "Connected").unwrap_or(false),
                    rssi: crate::prop(d, "RSSI"),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(b.rssi.unwrap_or(i16::MIN).cmp(&a.rssi.unwrap_or(i16::MIN)))
                .then(a.name.cmp(&b.name))
        });
        Ok(out)
    }

    fn ensure_agent(&mut self) -> Result<()> {
        if self.agent_registered {
            return Ok(());
        }
        self.conn.object_server().at(AGENT_PATH, AutoAgent)?;
        let mgr = Proxy::new(&self.conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1")?;
        let path = zbus::zvariant::ObjectPath::try_from(AGENT_PATH)?;
        mgr.call_method("RegisterAgent", &(&path, "NoInputNoOutput"))?;
        let _ = mgr.call_method("RequestDefaultAgent", &(&path,));
        self.agent_registered = true;
        Ok(())
    }

    /// Pairs (if needed), trusts and connects a device.
    pub fn pair_and_connect(&mut self, path: &str) -> Result<()> {
        if let Err(e) = self.ensure_agent() {
            log::warn!("could not register Bluetooth agent: {e}");
        }
        let _ = self.stop_discovery();
        let d = self.device(path)?;
        if !d.get_property::<bool>("Paired").unwrap_or(false) {
            d.call_method("Pair", &()).context("pairing failed")?;
        }
        let _ = d.set_property("Trusted", true);
        d.call_method("Connect", &()).context("connecting failed")?;
        Ok(())
    }

    pub fn disconnect(&self, path: &str) -> Result<()> {
        self.device(path)?.call_method("Disconnect", &())?;
        Ok(())
    }

    pub fn forget(&self, path: &str) -> Result<()> {
        let dev = OwnedObjectPath::try_from(path.to_string())?;
        let a = self.adapter()?;
        if !path.starts_with(a.path().as_str()) {
            bail!("device does not belong to the adapter");
        }
        a.call_method("RemoveDevice", &(dev,))?;
        Ok(())
    }
}
