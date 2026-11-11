//! Core, engine-independent logic for Carlita.
//!
//! Nothing in this crate talks to Carla, X11 or D-Bus, so all of it can be
//! unit-tested on any machine.

pub mod catalog;
pub mod chain;
pub mod discovery;
pub mod midi;
pub mod paths;
pub mod plugin;
pub mod preset;
pub mod rig;
pub mod settings;

pub use catalog::{CatalogFilter, PluginCatalog};
pub use chain::{Chain, ChainSlot, SlotId};
pub use plugin::{BinaryArch, PluginCategory, PluginDescriptor, PluginFormat};
pub use preset::{Preset, PresetStore};
pub use rig::{Rig, Track, TrackId, TrackInput};
pub use settings::Settings;

/// Reads a JSON file, returning `Ok(None)` when it does not exist.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes a JSON file atomically (write to a temp file, then rename), so a
/// power cut on stage never leaves a half-written preset or settings file.
pub(crate) fn write_json<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
