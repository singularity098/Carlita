//! The remembered list of scanned plugins, with favorites and filtering.

use crate::plugin::{PluginCategory, PluginDescriptor, PluginFormat};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// Scanned plugins plus the user's favorites. Persisted between launches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginCatalog {
    pub plugins: Vec<PluginDescriptor>,
    #[serde(default)]
    pub favorites: BTreeSet<String>,
    /// Unix time of the last completed scan.
    #[serde(default)]
    pub scanned_at: u64,
}

/// What the "Add plugin" browser is currently showing.
#[derive(Debug, Clone, Default)]
pub struct CatalogFilter {
    pub text: String,
    pub format: Option<PluginFormat>,
    pub category: Option<PluginCategory>,
    pub favorites_only: bool,
}

impl PluginCatalog {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(crate::read_json(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::write_json(path, self)
    }

    /// Replaces the plugin list after a scan, keeping favorites.
    pub fn replace_plugins(&mut self, mut plugins: Vec<PluginDescriptor>) {
        plugins.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.format.label().cmp(b.format.label()))
        });
        plugins.dedup_by_key(|p| p.key());
        self.plugins = plugins;
        self.scanned_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
    }

    pub fn is_favorite(&self, p: &PluginDescriptor) -> bool {
        self.favorites.contains(&p.key())
    }

    /// Toggles a favorite; returns the new state.
    pub fn toggle_favorite(&mut self, key: &str) -> bool {
        if !self.favorites.remove(key) {
            self.favorites.insert(key.to_string());
            true
        } else {
            false
        }
    }

    pub fn find(&self, key: &str) -> Option<&PluginDescriptor> {
        self.plugins.iter().find(|p| p.key() == key)
    }

    /// Plugins matching `filter`, favorites first.
    pub fn filtered(&self, filter: &CatalogFilter) -> Vec<&PluginDescriptor> {
        let words: Vec<String> = filter
            .text
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut out: Vec<&PluginDescriptor> = self
            .plugins
            .iter()
            .filter(|p| filter.format.is_none_or(|f| p.format == f))
            .filter(|p| filter.category.is_none_or(|c| p.category == c))
            .filter(|p| !filter.favorites_only || self.is_favorite(p))
            .filter(|p| {
                if words.is_empty() {
                    return true;
                }
                let text = p.search_text();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .collect();
        out.sort_by_key(|p| !self.is_favorite(p));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str, format: PluginFormat, category: PluginCategory) -> PluginDescriptor {
        PluginDescriptor {
            format,
            arch: Default::default(),
            name: name.into(),
            label: name.to_lowercase(),
            maker: "Acme".into(),
            filename: String::new(),
            unique_id: 0,
            category,
            audio_ins: 2,
            audio_outs: 2,
            midi_ins: 0,
            midi_outs: 0,
            parameters: 0,
            is_synth: false,
            has_custom_ui: false,
            yabridge: false,
        }
    }

    #[test]
    fn filters_and_puts_favorites_first() {
        let mut c = PluginCatalog::default();
        c.replace_plugins(vec![
            plugin("Reverb", PluginFormat::Lv2, PluginCategory::Delay),
            plugin("Amp Sim", PluginFormat::Vst3, PluginCategory::Distortion),
            plugin("Delay", PluginFormat::Vst3, PluginCategory::Delay),
        ]);
        let delay_key = c.plugins.iter().find(|p| p.name == "Delay").unwrap().key();
        assert!(c.toggle_favorite(&delay_key));

        let all = c.filtered(&CatalogFilter::default());
        assert_eq!(all[0].name, "Delay");
        assert_eq!(all.len(), 3);

        let vst3 = c.filtered(&CatalogFilter {
            format: Some(PluginFormat::Vst3),
            ..Default::default()
        });
        assert_eq!(vst3.len(), 2);

        let text = c.filtered(&CatalogFilter {
            text: "acme amp".into(),
            ..Default::default()
        });
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].name, "Amp Sim");

        let favs = c.filtered(&CatalogFilter {
            favorites_only: true,
            ..Default::default()
        });
        assert_eq!(favs.len(), 1);

        let cat = c.filtered(&CatalogFilter {
            category: Some(PluginCategory::Delay),
            ..Default::default()
        });
        assert_eq!(cat.len(), 2);
    }

    #[test]
    fn round_trips_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.json");
        let mut c = PluginCatalog::default();
        c.replace_plugins(vec![plugin(
            "Reverb",
            PluginFormat::Lv2,
            PluginCategory::Delay,
        )]);
        c.save(&path).unwrap();
        let loaded = PluginCatalog::load(&path).unwrap();
        assert_eq!(loaded.plugins, c.plugins);
        assert!(PluginCatalog::load(&dir.path().join("missing.json"))
            .unwrap()
            .plugins
            .is_empty());
    }
}
