//! MIDI input for MIDI learn, read with midir (ALSA sequencer; PipeWire and
//! JACK bridge their MIDI devices to it).
//!
//! The audio path does not depend on this: hardware MIDI reaches the plugins
//! through Carla's own MIDI input. These listeners only watch for the
//! footswitches/controllers mapped to bypass, and for which devices send
//! something while a track's inputs are being picked.

use crossbeam_channel::{unbounded, Receiver, Sender};
use midir::{MidiInput, MidiInputConnection};

pub struct MidiListener {
    /// The client name the connections show under.
    client: &'static str,
    connections: Vec<MidiInputConnection<()>>,
    tx: Sender<(String, Vec<u8>)>,
    /// Each message, with the name of the port it came in on.
    pub messages: Receiver<(String, Vec<u8>)>,
}

impl MidiListener {
    pub fn new(client: &'static str) -> Self {
        let (tx, rx) = unbounded();
        Self {
            client,
            connections: Vec::new(),
            tx,
            messages: rx,
        }
    }

    /// Stops listening.
    pub fn stop(&mut self) {
        self.connections.clear();
    }

    pub fn is_listening(&self) -> bool {
        !self.connections.is_empty()
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
            let Ok(mut input) = MidiInput::new(self.client) else {
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
            let port_name = name.clone();
            match input.connect(
                &p,
                self.client,
                move |_, bytes, _| {
                    let _ = tx.send((port_name.clone(), bytes.to_vec()));
                },
                (),
            ) {
                Ok(c) => self.connections.push(c),
                Err(e) => log::warn!("cannot open MIDI input {name}: {e}"),
            }
        }
        log::info!(
            "{} listening on {} port(s)",
            self.client,
            self.connections.len()
        );
    }
}
