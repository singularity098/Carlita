//! Carlita's audio engine: a thin, safe layer over Carla's host API.
//!
//! All Carla calls happen on one dedicated engine thread. The UI talks to it
//! through [`EngineHandle::send`] and receives [`Event`]s, so slow operations
//! (loading a bridged plugin can take seconds) never block the UI, and the UI
//! never touches the audio thread – Carla owns that.
//!
//! The chain runs in Carla's *continuous rack* mode: plugins are processed in
//! series in the order of their Carla ids, audio and MIDI flow from each
//! plugin to the next, and adding, removing or swapping plugins happens while
//! audio keeps running.

mod carla;
mod thread;
mod types;

pub use thread::EngineHandle;
pub use types::*;
