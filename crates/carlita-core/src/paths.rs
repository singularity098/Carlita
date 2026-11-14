//! Standard locations for Carlita's files (XDG on Linux).

use std::path::PathBuf;

/// Where Carlita keeps its files.
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// `~/.config/carlita` – settings.
    pub config_dir: PathBuf,
    /// `~/.local/share/carlita` – presets.
    pub data_dir: PathBuf,
    /// `~/.cache/carlita` – plugin scan cache.
    pub cache_dir: PathBuf,
    /// `~/Music/Carlita` – recording projects, unless the user picks
    /// another folder.
    pub default_projects_dir: PathBuf,
}

impl AppPaths {
    /// Resolves the XDG directories. `CARLITA_HOME` overrides all of them
    /// (handy for tests and for running several instances side by side).
    pub fn resolve() -> Self {
        if let Some(home) = std::env::var_os("CARLITA_HOME") {
            let home = PathBuf::from(home);
            return Self::rooted(home);
        }
        match directories::ProjectDirs::from("", "", "carlita") {
            Some(dirs) => Self {
                config_dir: dirs.config_dir().to_path_buf(),
                data_dir: dirs.data_dir().to_path_buf(),
                cache_dir: dirs.cache_dir().to_path_buf(),
                default_projects_dir: directories::UserDirs::new()
                    .map(|u| {
                        u.audio_dir()
                            .map(|a| a.to_path_buf())
                            .unwrap_or_else(|| u.home_dir().join("Music"))
                    })
                    .unwrap_or_else(|| dirs.data_dir().to_path_buf())
                    .join("Carlita"),
            },
            None => Self::rooted(std::env::temp_dir().join("carlita")),
        }
    }

    /// All directories below one root.
    pub fn rooted(root: PathBuf) -> Self {
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            default_projects_dir: root.join("projects"),
        }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn presets_dir(&self) -> PathBuf {
        self.data_dir.join("presets")
    }

    pub fn plugin_cache_file(&self) -> PathBuf {
        self.cache_dir.join("plugins.json")
    }

    /// What the last plugin scan found, and why plugins failed.
    pub fn scan_log_file(&self) -> PathBuf {
        self.cache_dir.join("scan.log")
    }
}
