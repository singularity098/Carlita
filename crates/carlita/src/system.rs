//! Background worker for WiFi, Bluetooth, brightness and power actions, so
//! slow D-Bus calls never block the UI.

use carlita_system::bluetooth::{Bluetooth, BtDevice};
use carlita_system::power::{self, Backlight};
use carlita_system::wifi::{Wifi, WifiNetwork};
use crossbeam_channel::{unbounded, Receiver, Sender};

pub enum SysCmd {
    RefreshWifi,
    RefreshBt,
    SetWifi(bool),
    ScanWifi,
    ConnectWifi {
        ssid: String,
        password: Option<String>,
    },
    DisconnectWifi,
    SetBt(bool),
    ScanBt(bool),
    ConnectBt(String),
    DisconnectBt(String),
    ForgetBt(String),
    SetBrightness(u32),
    PowerOff,
    Reboot,
}

pub enum SysEvent {
    Wifi {
        available: bool,
        enabled: bool,
        networks: Vec<WifiNetwork>,
        status: String,
    },
    Bt {
        available: bool,
        enabled: bool,
        devices: Vec<BtDevice>,
        scanning: bool,
        status: String,
    },
    Brightness(Option<u32>),
    Info(String),
    Error(String),
}

pub struct SystemWorker {
    tx: Sender<SysCmd>,
    pub events: Receiver<SysEvent>,
}

impl SystemWorker {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded::<SysCmd>();
        let (ev_tx, ev_rx) = unbounded();
        std::thread::Builder::new()
            .name("carlita-system".into())
            .spawn(move || {
                Worker {
                    events: ev_tx,
                    wifi: None,
                    bt: None,
                    bt_scanning: false,
                    wifi_status: String::new(),
                    bt_status: String::new(),
                }
                .run(rx)
            })
            .expect("spawn system worker");
        Self { tx, events: ev_rx }
    }

    pub fn send(&self, cmd: SysCmd) {
        let _ = self.tx.send(cmd);
    }
}

struct Worker {
    events: Sender<SysEvent>,
    wifi: Option<Wifi>,
    bt: Option<Bluetooth>,
    bt_scanning: bool,
    wifi_status: String,
    bt_status: String,
}

impl Worker {
    fn run(mut self, rx: Receiver<SysCmd>) {
        let _ = self.events.send(SysEvent::Brightness(
            Backlight::find().and_then(|b| b.percent().ok()),
        ));
        while let Ok(cmd) = rx.recv() {
            self.handle(cmd);
        }
    }

    fn wifi(&mut self) -> Option<&Wifi> {
        if self.wifi.is_none() {
            self.wifi = Wifi::connect().ok();
        }
        self.wifi.as_ref()
    }

    fn bt(&mut self) -> Option<&mut Bluetooth> {
        if self.bt.is_none() {
            self.bt = Bluetooth::connect().ok();
        }
        self.bt.as_mut()
    }

    fn error(&self, what: &str, e: anyhow::Error) {
        log::warn!("{what}: {e:#}");
        let _ = self.events.send(SysEvent::Error(format!("{what}: {e}")));
    }

    fn handle(&mut self, cmd: SysCmd) {
        match cmd {
            SysCmd::RefreshWifi => self.report_wifi(),
            SysCmd::RefreshBt => self.report_bt(),
            SysCmd::SetWifi(on) => {
                if let Some(Err(e)) = self.wifi().map(|w| w.set_enabled(on)) {
                    self.error("WiFi", e);
                }
                self.report_wifi();
            }
            SysCmd::ScanWifi => {
                if let Some(Err(e)) = self.wifi().map(|w| w.request_scan()) {
                    self.error("WiFi scan", e);
                }
                self.wifi_status = "Scanning…".into();
                self.report_wifi();
            }
            SysCmd::ConnectWifi { ssid, password } => {
                self.wifi_status = format!("Connecting to {ssid}…");
                self.report_wifi();
                let result = self.wifi().map(|w| {
                    let nets = w.networks()?;
                    let net = nets
                        .iter()
                        .find(|n| n.ssid == ssid)
                        .ok_or_else(|| anyhow::anyhow!("{ssid} is no longer visible"))?;
                    w.connect_to(net, password.as_deref())
                });
                match result {
                    Some(Ok(())) => {
                        self.wifi_status = format!("Connecting to {ssid}");
                        let _ = self
                            .events
                            .send(SysEvent::Info(format!("Connecting to {ssid}")));
                    }
                    Some(Err(e)) => {
                        self.wifi_status.clear();
                        self.error("WiFi", e);
                    }
                    None => {}
                }
                self.report_wifi();
            }
            SysCmd::DisconnectWifi => {
                if let Some(Err(e)) = self.wifi().map(|w| w.disconnect()) {
                    self.error("WiFi", e);
                }
                self.report_wifi();
            }
            SysCmd::SetBt(on) => {
                if let Some(Err(e)) = self.bt().map(|b| b.set_powered(on)) {
                    self.error("Bluetooth", e);
                }
                self.report_bt();
            }
            SysCmd::ScanBt(on) => {
                let r = self.bt().map(|b| {
                    if on {
                        b.start_discovery()
                    } else {
                        b.stop_discovery()
                    }
                });
                match r {
                    Some(Ok(())) => self.bt_scanning = on,
                    Some(Err(e)) => self.error("Bluetooth", e),
                    None => {}
                }
                self.report_bt();
            }
            SysCmd::ConnectBt(path) => {
                self.bt_status = "Connecting…".into();
                self.report_bt();
                self.bt_scanning = false;
                match self.bt().map(|b| b.pair_and_connect(&path)) {
                    Some(Ok(())) => self.bt_status = "Connected".into(),
                    Some(Err(e)) => {
                        self.bt_status.clear();
                        self.error("Bluetooth", e);
                    }
                    None => {}
                }
                self.report_bt();
            }
            SysCmd::DisconnectBt(path) => {
                if let Some(Err(e)) = self.bt().map(|b| b.disconnect(&path)) {
                    self.error("Bluetooth", e);
                }
                self.report_bt();
            }
            SysCmd::ForgetBt(path) => {
                if let Some(Err(e)) = self.bt().map(|b| b.forget(&path)) {
                    self.error("Bluetooth", e);
                }
                self.report_bt();
            }
            SysCmd::SetBrightness(p) => match Backlight::find() {
                Some(b) => {
                    if let Err(e) = b.set_percent(p) {
                        self.error("Brightness", e);
                    }
                }
                None => {
                    let _ = self.events.send(SysEvent::Brightness(None));
                }
            },
            SysCmd::PowerOff => {
                if let Err(e) = power::power_off() {
                    self.error("Shut down", e);
                }
            }
            SysCmd::Reboot => {
                if let Err(e) = power::reboot() {
                    self.error("Reboot", e);
                }
            }
        }
    }

    fn report_wifi(&mut self) {
        let status = self.wifi_status.clone();
        let ev = match self.wifi() {
            Some(w) if w.is_available() => {
                let enabled = w.enabled().unwrap_or(false);
                let networks = if enabled {
                    w.networks().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let status = match networks.iter().find(|n| n.active) {
                    Some(n) => format!("Connected to {}", n.ssid),
                    None if !enabled => "Off".into(),
                    None if status.is_empty() => "Not connected".into(),
                    None => status,
                };
                SysEvent::Wifi {
                    available: true,
                    enabled,
                    networks,
                    status,
                }
            }
            _ => SysEvent::Wifi {
                available: false,
                enabled: false,
                networks: Vec::new(),
                status: String::new(),
            },
        };
        let _ = self.events.send(ev);
    }

    fn report_bt(&mut self) {
        let scanning = self.bt_scanning;
        let status = self.bt_status.clone();
        let ev = match self.bt() {
            Some(b) if b.is_available() => {
                let enabled = b.powered().unwrap_or(false);
                let devices = if enabled {
                    b.devices().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let status = if !enabled {
                    "Off".into()
                } else if scanning {
                    "Searching…".into()
                } else if let Some(d) = devices.iter().find(|d| d.connected) {
                    format!("Connected to {}", d.name)
                } else if status.is_empty() {
                    "On".into()
                } else {
                    status
                };
                SysEvent::Bt {
                    available: true,
                    enabled,
                    devices,
                    scanning: scanning && enabled,
                    status,
                }
            }
            _ => SysEvent::Bt {
                available: false,
                enabled: false,
                devices: Vec::new(),
                scanning: false,
                status: String::new(),
            },
        };
        let _ = self.events.send(ev);
    }
}
