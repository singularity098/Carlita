//! Carlita's audio engine: a thin, safe layer over Carla's host API.
//!
//! All Carla calls happen on one dedicated engine thread. The UI talks to it
//! through [`EngineHandle::send`] and receives [`Event`]s, so slow operations
//! (loading a bridged plugin can take seconds) never block the UI, and the UI
//! never touches the audio thread – Carla owns that.
//!
//! Carla runs in *patchbay* mode. The UI sends the rig's layout (tracks,
//! their chains and parallel blocks) and the engine connects the plugins in
//! Carla's internal graph to match, changing only the connections that
//! differ, so plugins are added, moved and bypassed while audio keeps running.

mod carla;
mod thread;
mod types;

pub use thread::EngineHandle;
pub use types::*;
