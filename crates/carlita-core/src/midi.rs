//! MIDI learn: mapping footswitches / controllers to plugin bypass.

use crate::chain::SlotId;
use serde::{Deserialize, Serialize};

/// A parsed, channel voice MIDI message (only what MIDI learn needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage {
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        note: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
}

impl MidiMessage {
    /// Parses raw MIDI bytes. Note-on with velocity 0 is a note-off.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let status = *bytes.first()?;
        let channel = status & 0x0F;
        let d1 = bytes.get(1).copied().unwrap_or(0) & 0x7F;
        let d2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
        match status & 0xF0 {
            0x80 if bytes.len() >= 3 => Some(MidiMessage::NoteOff { channel, note: d1 }),
            0x90 if bytes.len() >= 3 && d2 == 0 => Some(MidiMessage::NoteOff { channel, note: d1 }),
            0x90 if bytes.len() >= 3 => Some(MidiMessage::NoteOn {
                channel,
                note: d1,
                velocity: d2,
            }),
            0xB0 if bytes.len() >= 3 => Some(MidiMessage::ControlChange {
                channel,
                controller: d1,
                value: d2,
            }),
            0xC0 if bytes.len() >= 2 => Some(MidiMessage::ProgramChange {
                channel,
                program: d1,
            }),
            _ => None,
        }
    }

    /// The trigger this message would be learned as, and whether it is a
    /// "press" (CC ≥ 64 / note on) or a "release".
    pub fn trigger(&self) -> Option<(MidiTrigger, bool)> {
        match *self {
            MidiMessage::NoteOn { channel, note, .. } => {
                Some((MidiTrigger::Note { channel, note }, true))
            }
            MidiMessage::NoteOff { channel, note } => {
                Some((MidiTrigger::Note { channel, note }, false))
            }
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => Some((
                MidiTrigger::Cc {
                    channel,
                    controller,
                },
                value >= 64,
            )),
            MidiMessage::ProgramChange { channel, program } => {
                Some((MidiTrigger::Program { channel, program }, true))
            }
        }
    }
}

/// The MIDI event a mapping listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MidiTrigger {
    Cc { channel: u8, controller: u8 },
    Note { channel: u8, note: u8 },
    Program { channel: u8, program: u8 },
}

impl MidiTrigger {
    pub fn label(&self) -> String {
        match self {
            MidiTrigger::Cc {
                channel,
                controller,
            } => format!("CC {controller} · ch {}", channel + 1),
            MidiTrigger::Note { channel, note } => format!("Note {note} · ch {}", channel + 1),
            MidiTrigger::Program { channel, program } => {
                format!("PC {program} · ch {}", channel + 1)
            }
        }
    }
}

/// What a mapping does to its plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MidiAction {
    /// Each press toggles bypass (latching footswitch behaviour).
    #[default]
    Toggle,
    /// Press = enabled, release = bypassed (momentary / follows the switch).
    Follow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiMapping {
    pub trigger: MidiTrigger,
    pub slot: SlotId,
    #[serde(default)]
    pub action: MidiAction,
}

/// Bypass change requested by incoming MIDI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassChange {
    Toggle(SlotId),
    Set { slot: SlotId, bypassed: bool },
}

/// Resolves which bypass changes a message triggers.
pub fn resolve(mappings: &[MidiMapping], msg: &MidiMessage) -> Vec<BypassChange> {
    let Some((trigger, pressed)) = msg.trigger() else {
        return Vec::new();
    };
    mappings
        .iter()
        .filter(|m| m.trigger == trigger)
        .filter_map(|m| match m.action {
            MidiAction::Toggle if pressed => Some(BypassChange::Toggle(m.slot)),
            MidiAction::Toggle => None,
            MidiAction::Follow => Some(BypassChange::Set {
                slot: m.slot,
                bypassed: !pressed,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages() {
        assert_eq!(
            MidiMessage::parse(&[0x91, 60, 100]),
            Some(MidiMessage::NoteOn {
                channel: 1,
                note: 60,
                velocity: 100
            })
        );
        assert_eq!(
            MidiMessage::parse(&[0x90, 60, 0]),
            Some(MidiMessage::NoteOff {
                channel: 0,
                note: 60
            })
        );
        assert_eq!(
            MidiMessage::parse(&[0xB0, 64, 127]),
            Some(MidiMessage::ControlChange {
                channel: 0,
                controller: 64,
                value: 127
            })
        );
        assert_eq!(MidiMessage::parse(&[0xF8]), None);
        assert_eq!(MidiMessage::parse(&[0xB0, 64]), None);
    }

    #[test]
    fn toggle_fires_on_press_only() {
        let m = [MidiMapping {
            trigger: MidiTrigger::Cc {
                channel: 0,
                controller: 80,
            },
            slot: SlotId(3),
            action: MidiAction::Toggle,
        }];
        let press = MidiMessage::parse(&[0xB0, 80, 127]).unwrap();
        let release = MidiMessage::parse(&[0xB0, 80, 0]).unwrap();
        assert_eq!(resolve(&m, &press), vec![BypassChange::Toggle(SlotId(3))]);
        assert!(resolve(&m, &release).is_empty());
        let other = MidiMessage::parse(&[0xB1, 80, 127]).unwrap();
        assert!(resolve(&m, &other).is_empty());
    }

    #[test]
    fn follow_tracks_switch_state() {
        let m = [MidiMapping {
            trigger: MidiTrigger::Note {
                channel: 9,
                note: 36,
            },
            slot: SlotId(1),
            action: MidiAction::Follow,
        }];
        let on = MidiMessage::parse(&[0x99, 36, 90]).unwrap();
        let off = MidiMessage::parse(&[0x89, 36, 0]).unwrap();
        assert_eq!(
            resolve(&m, &on),
            vec![BypassChange::Set {
                slot: SlotId(1),
                bypassed: false
            }]
        );
        assert_eq!(
            resolve(&m, &off),
            vec![BypassChange::Set {
                slot: SlotId(1),
                bypassed: true
            }]
        );
    }
}
