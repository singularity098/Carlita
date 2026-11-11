//! Named presets: the whole rig (every track and its chain) including each
//! plugin's state.

use crate::chain::Chain;
use crate::midi::MidiMapping;
use crate::rig::Rig;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Version 1 held a single chain; version 2 holds a rig of tracks.
pub const PRESET_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub rig: Rig,
    /// The single chain of a version 1 preset; moved into `rig` on load.
    #[serde(default, skip_serializing)]
    chain: Option<Chain>,
    #[serde(default)]
    pub midi_mappings: Vec<MidiMapping>,
}

fn default_version() -> u32 {
    PRESET_VERSION
}

impl Preset {
    pub fn new(name: impl Into<String>, rig: Rig, midi_mappings: Vec<MidiMapping>) -> Self {
        Self {
            name: name.into(),
            version: PRESET_VERSION,
            rig,
            chain: None,
            midi_mappings,
        }
    }

    /// Upgrades a version 1 preset and makes sure there is a track.
    fn migrate(&mut self) {
        if let Some(chain) = self.chain.take() {
            if self.rig.tracks.is_empty() {
                self.rig = Rig::from_chain(chain);
            }
        }
        self.rig.ensure_track();
    }
}

/// Presets stored as one JSON file each in a directory.
#[derive(Debug, Clone)]
pub struct PresetStore {
    dir: PathBuf,
}

impl PresetStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, name: &str) -> anyhow::Result<PathBuf> {
        let name = name.trim();
        if name.is_empty() {
            bail!("preset name is empty");
        }
        if name.chars().any(|c| matches!(c, '/' | '\\' | '\0')) || name == "." || name == ".." {
            bail!("preset name contains invalid characters");
        }
        Ok(self.dir.join(format!("{name}.json")))
    }

    /// Preset names, sorted case-insensitively.
    pub fn list(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "json" {
                    return None;
                }
                Some(p.file_stem()?.to_string_lossy().into_owned())
            })
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    pub fn exists(&self, name: &str) -> bool {
        self.path_for(name).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn load(&self, name: &str) -> anyhow::Result<Preset> {
        let path = self.path_for(name)?;
        let mut preset: Preset =
            crate::read_json(&path)?.with_context(|| format!("preset '{name}' not found"))?;
        if preset.version > PRESET_VERSION {
            bail!(
                "preset '{name}' was saved by a newer Carlita (version {})",
                preset.version
            );
        }
        preset.name = name.trim().to_string();
        preset.migrate();
        Ok(preset)
    }

    pub fn save(&self, preset: &Preset) -> anyhow::Result<()> {
        let path = self.path_for(&preset.name)?;
        crate::write_json(&path, preset)
    }

    pub fn rename(&self, old: &str, new: &str) -> anyhow::Result<()> {
        let from = self.path_for(old)?;
        let to = self.path_for(new)?;
        if from == to {
            return Ok(());
        }
        if to.exists() {
            bail!("a preset called '{}' already exists", new.trim());
        }
        let mut preset = self.load(old)?;
        preset.name = new.trim().to_string();
        crate::write_json(&to, &preset)?;
        std::fs::remove_file(from)?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<()> {
        std::fs::remove_file(self.path_for(name)?)
            .with_context(|| format!("deleting preset '{name}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_list_rename_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = PresetStore::new(dir.path().join("presets"));
        assert!(store.list().is_empty());

        store
            .save(&Preset::new("Clean", Rig::new(), vec![]))
            .unwrap();
        store
            .save(&Preset::new("ambient", Rig::new(), vec![]))
            .unwrap();
        assert_eq!(store.list(), ["ambient", "Clean"]);

        store.rename("Clean", "Crunch").unwrap();
        assert_eq!(store.load("Crunch").unwrap().name, "Crunch");
        assert!(!store.exists("Clean"));
        assert!(store.rename("Crunch", "ambient").is_err());

        store.delete("ambient").unwrap();
        assert_eq!(store.list(), ["Crunch"]);
    }

    #[test]
    fn loads_version_1_presets() {
        let dir = tempfile::tempdir().unwrap();
        let store = PresetStore::new(dir.path());
        let v1 = r#"{"name":"Old","version":1,"chain":{"slots":[{"id":3,"plugin":{
            "format":"internal","name":"Gain","label":"audiogain_s","filename":"",
            "unique_id":0,"category":"utility","audio_ins":2,"audio_outs":2,
            "midi_ins":0,"midi_outs":0,"parameters":1,"is_synth":false,
            "has_custom_ui":false},"name":"Gain","bypassed":true}],"next_id":4},
            "midi_mappings":[]}"#;
        std::fs::write(dir.path().join("Old.json"), v1).unwrap();
        let p = store.load("Old").unwrap();
        assert_eq!(p.rig.tracks.len(), 1);
        let s = p.rig.slots().next().unwrap();
        assert_eq!((s.id.0, s.bypassed), (3, true));
        // Saved again as version 2, without the old chain.
        store.save(&p).unwrap();
        let text = std::fs::read_to_string(dir.path().join("Old.json")).unwrap();
        assert!(text.contains("\"rig\"") && !text.contains("\"chain\""));
    }

    #[test]
    fn rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = PresetStore::new(dir.path());
        assert!(store
            .save(&Preset::new("../evil", Rig::new(), vec![]))
            .is_err());
        assert!(store.save(&Preset::new("  ", Rig::new(), vec![])).is_err());
    }
}
