//! MIDI input for MIDI learn, read with midir (ALSA sequencer; PipeWire and
//! JACK bridge their MIDI devices to it).
//!
//! The audio path does not depend on this: hardware MIDI reaches the plugins
//! through Carla's own MIDI input. This listener only watches for the
//! footswitches/controllers mapped to bypass.

use crossbeam_channel::{unbounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection};

pub struct MidiListener {
    connections: Vec<MidiInputConnection<()>>,
    tx: Sender<Vec<u8>>,
    pub messages: Receiver<Vec<u8>>,
}

impl MidiListener {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            connections: Vec::new(),
            tx,
            messages: rx,
        }
    }

    /// Names of the available MIDI input ports.
    pub fn ports() -> Vec<String> {
        let Ok(input) = MidiInput::new("Carlita (scan)") else {
            return Vec::new();
        };
        input
            .ports()
            .iter()
            .filter_map(|p| input.port_name(p).ok())
            .filter(|n| !n.contains("Carlita") && !n.contains("Midi Through"))
            .collect()
    }

    /// Listens on the named port, or on every port when `port` is empty.
    pub fn listen(&mut self, port: &str) {
        self.connections.clear();
        let names = Self::ports();
        for name in names
            .iter()
            .filter(|n| port.is_empty() || n.as_str() == port)
        {
            let Ok(mut input) = MidiInput::new("Carlita MIDI learn") else {
                continue;
            };
            input.ignore(midir::Ignore::All);
            let Some(p) = input
                .ports()
                .into_iter()
                .find(|p| input.port_name(p).ok().as_deref() == Some(name))
            else {
                continue;
            };
            let tx = self.tx.clone();
            match input.connect(
                &p,
                "carlita-learn",
                move |_, bytes, _| {
                    let _ = tx.send(bytes.to_vec());
                },
                (),
            ) {
                Ok(c) => self.connections.push(c),
                Err(e) => log::warn!("cannot open MIDI input {name}: {e}"),
            }
        }
        log::info!("MIDI learn listening on {} port(s)", self.connections.len());
    }
}
