//! Plugin scanning through Carla's `carla-discovery-native` tool.
//!
//! Every plugin binary is probed in a separate `carla-discovery` process with
//! a timeout, so a plugin that crashes or hangs while being scanned cannot
//! take the app down; it is simply reported as failed.

use crate::plugin::{PluginCategory, PluginDescriptor, PluginFormat};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PREFIX: &str = "carla-discovery::";

/// Carla's `PLUGIN_IS_SYNTH` hint.
const HINT_IS_SYNTH: u32 = 0x004;
/// Carla's `PLUGIN_HAS_CUSTOM_UI` hint.
const HINT_HAS_CUSTOM_UI: u32 = 0x008;

/// Parses the text printed by `carla-discovery-*` into plugin descriptors.
///
/// `filename` is the path that was probed (empty for `:all` scans).
pub fn parse_discovery_output(
    format: PluginFormat,
    filename: &str,
    output: &str,
) -> Vec<PluginDescriptor> {
    let mut plugins = Vec::new();
    let mut current: Option<PluginDescriptor> = None;

    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix(PREFIX) else {
            continue;
        };
        let (key, value) = rest.split_once("::").unwrap_or((rest, ""));
        match key {
            "init" => current = Some(empty_descriptor(format, filename)),
            "end" => {
                if let Some(mut p) = current.take() {
                    finish(&mut p);
                    plugins.push(p);
                }
            }
            _ => {
                let Some(p) = current.as_mut() else { continue };
                let num = || value.trim().parse::<i64>().unwrap_or(0);
                match key {
                    "name" => p.name = value.to_string(),
                    "label" => p.label = value.to_string(),
                    "maker" => p.maker = value.to_string(),
                    "category" => p.category = PluginCategory::from_discovery(value),
                    "uniqueId" => p.unique_id = num(),
                    "hints" => {
                        let hints = num() as u32;
                        p.is_synth = hints & HINT_IS_SYNTH != 0;
                        p.has_custom_ui = hints & HINT_HAS_CUSTOM_UI != 0;
                    }
                    "audio.ins" => p.audio_ins = num() as u32,
                    "audio.outs" => p.audio_outs = num() as u32,
                    "midi.ins" => p.midi_ins = num() as u32,
                    "midi.outs" => p.midi_outs = num() as u32,
                    "parameters.ins" => p.parameters = num() as u32,
                    _ => {}
                }
            }
        }
    }
    plugins
}

fn empty_descriptor(format: PluginFormat, filename: &str) -> PluginDescriptor {
    PluginDescriptor {
        format,
        arch: Default::default(),
        name: String::new(),
        label: String::new(),
        maker: String::new(),
        filename: filename.to_string(),
        unique_id: 0,
        category: PluginCategory::Other,
        audio_ins: 0,
        audio_outs: 0,
        midi_ins: 0,
        midi_outs: 0,
        parameters: 0,
        is_synth: false,
        has_custom_ui: false,
        yabridge: filename.contains("/yabridge/") || filename.contains("/yabridge-"),
    }
}

fn finish(p: &mut PluginDescriptor) {
    if p.format == PluginFormat::Lv2 {
        // Discovery prints LV2 labels as "<bundle>/<uri>"; Carla loads LV2
        // plugins by URI alone.
        if let Some((bundle, uri)) = p.label.split_once('/') {
            if !bundle.contains(':') {
                p.label = uri.to_string();
            }
        }
    }
    if p.is_synth {
        p.category = PluginCategory::Synth;
    }
    if p.name.is_empty() {
        p.name = if !p.label.is_empty() {
            p.label.clone()
        } else {
            Path::new(&p.filename)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Unnamed plugin".into())
        };
    }
}

/// Default search paths for one format, including yabridge's directories.
pub fn default_search_paths(format: PluginFormat) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let env_paths = |var: &str| -> Vec<PathBuf> {
        std::env::var_os(var)
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default()
    };
    let mut paths = match format {
        PluginFormat::Vst2 => {
            let mut p = env_paths("VST_PATH");
            p.extend([
                home.join(".vst"),
                home.join(".lxvst"),
                "/usr/lib/vst".into(),
                "/usr/local/lib/vst".into(),
                "/usr/lib/lxvst".into(),
            ]);
            p
        }
        PluginFormat::Vst3 => {
            let mut p = env_paths("VST3_PATH");
            p.extend([
                home.join(".vst3"),
                "/usr/lib/vst3".into(),
                "/usr/local/lib/vst3".into(),
            ]);
            p
        }
        PluginFormat::Clap => {
            let mut p = env_paths("CLAP_PATH");
            p.extend([
                home.join(".clap"),
                "/usr/lib/clap".into(),
                "/usr/local/lib/clap".into(),
            ]);
            p
        }
        PluginFormat::Ladspa => {
            let mut p = env_paths("LADSPA_PATH");
            p.extend([
                home.join(".ladspa"),
                "/usr/lib/ladspa".into(),
                "/usr/local/lib/ladspa".into(),
            ]);
            p
        }
        PluginFormat::Lv2 | PluginFormat::Internal => Vec::new(),
    };
    paths.dedup();
    paths
}

/// Finds plugin binaries/bundles for one format below `roots`.
pub fn find_candidates(format: PluginFormat, roots: &[PathBuf]) -> Vec<PathBuf> {
    let ext = match format {
        PluginFormat::Vst2 | PluginFormat::Ladspa => "so",
        PluginFormat::Vst3 => "vst3",
        PluginFormat::Clap => "clap",
        PluginFormat::Lv2 | PluginFormat::Internal => return Vec::new(),
    };
    let mut found = Vec::new();
    for root in roots {
        walk(root, ext, &mut found, 0);
    }
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let matches = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext));
        if matches {
            // VST3/CLAP bundles are directories: take them whole.
            out.push(path);
        } else if path.is_dir() {
            walk(&path, ext, out, depth + 1);
        }
    }
}

/// Result of probing one binary.
#[derive(Debug)]
pub enum ProbeResult {
    Ok(Vec<PluginDescriptor>),
    Failed(String),
    TimedOut,
}

/// Runs `carla-discovery-native <format> <target>` with a timeout.
pub fn probe(
    discovery_tool: &Path,
    format: PluginFormat,
    target: &str,
    timeout: Duration,
) -> ProbeResult {
    let child = Command::new(discovery_tool)
        .arg(format.discovery_arg())
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult::Failed(format!("cannot run {}: {e}", discovery_tool.display()))
        }
    };

    // Read stdout on a helper thread so a chatty plugin cannot fill the pipe
    // and deadlock while we wait for it.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeResult::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return ProbeResult::Failed(e.to_string()),
        }
    }
    let output = reader.join().unwrap_or_default();
    let filename = if target.starts_with(':') { "" } else { target };
    let plugins = parse_discovery_output(format, filename, &output);
    if plugins.is_empty() {
        let err = output
            .lines()
            .find_map(|l| l.trim().strip_prefix("carla-discovery::error::"))
            .unwrap_or("no plugins found");
        ProbeResult::Failed(err.to_string())
    } else {
        ProbeResult::Ok(plugins)
    }
}

/// Progress reported while scanning.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

/// Outcome of a full scan.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub plugins: Vec<PluginDescriptor>,
    pub failures: Vec<(String, String)>,
}

/// Scans all formats. `progress` is called before each probe.
pub fn scan_all(
    discovery_tool: &Path,
    formats: &[PluginFormat],
    timeout: Duration,
    mut progress: impl FnMut(ScanProgress),
) -> ScanReport {
    let mut jobs: Vec<(PluginFormat, String)> = Vec::new();
    for &format in formats {
        match format {
            PluginFormat::Lv2 | PluginFormat::Internal => jobs.push((format, ":all".into())),
            _ => {
                for c in find_candidates(format, &default_search_paths(format)) {
                    jobs.push((format, c.to_string_lossy().into_owned()));
                }
            }
        }
    }

    let mut report = ScanReport::default();
    let total = jobs.len();
    for (i, (format, target)) in jobs.into_iter().enumerate() {
        progress(ScanProgress {
            done: i,
            total,
            current: target.clone(),
        });
        // LV2 ":all" scans everything at once, so give it longer.
        let t = if target.starts_with(':') {
            timeout * 6
        } else {
            timeout
        };
        match probe(discovery_tool, format, &target, t) {
            ProbeResult::Ok(p) => report.plugins.extend(p),
            ProbeResult::Failed(e) => report.failures.push((target, e)),
            ProbeResult::TimedOut => report.failures.push((target, "timed out".into())),
        }
    }
    progress(ScanProgress {
        done: total,
        total,
        current: String::new(),
    });
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
carla-discovery::init::-----------
carla-discovery::build::2
carla-discovery::hints::2
carla-discovery::category::utility
carla-discovery::name::Audio Gain (Mono)
carla-discovery::maker::falkTX
carla-discovery::label::audiogain
carla-discovery::audio.ins::1
carla-discovery::audio.outs::1
carla-discovery::midi.ins::0
carla-discovery::parameters.ins::1
carla-discovery::end::------------
carla-discovery::init::-----------
carla-discovery::hints::12
carla-discovery::category::other
carla-discovery::name::Big Synth
carla-discovery::uniqueId::1234
carla-discovery::midi.ins::1
carla-discovery::audio.outs::2
carla-discovery::end::------------
";

    #[test]
    fn parses_multiple_plugins() {
        let p = parse_discovery_output(PluginFormat::Internal, "", SAMPLE);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "Audio Gain (Mono)");
        assert_eq!(p[0].label, "audiogain");
        assert_eq!(p[0].category, PluginCategory::Utility);
        assert_eq!(p[0].audio_ins, 1);
        assert!(!p[0].is_synth);
        assert!(p[1].is_synth);
        assert!(p[1].has_custom_ui);
        assert_eq!(p[1].category, PluginCategory::Synth);
        assert_eq!(p[1].unique_id, 1234);
    }

    #[test]
    fn lv2_label_is_reduced_to_uri() {
        let out = "carla-discovery::init::-\ncarla-discovery::label::foo.lv2/http://example.org/foo\ncarla-discovery::end::-\n";
        let p = parse_discovery_output(PluginFormat::Lv2, "", out);
        assert_eq!(p[0].label, "http://example.org/foo");
        assert_eq!(p[0].name, "http://example.org/foo");
    }

    #[test]
    fn yabridge_paths_are_flagged() {
        let out =
            "carla-discovery::init::-\ncarla-discovery::name::Win Plug\ncarla-discovery::end::-\n";
        let p = parse_discovery_output(PluginFormat::Vst3, "/home/u/.vst3/yabridge/Plug.vst3", out);
        assert!(p[0].yabridge);
    }

    #[test]
    fn finds_bundles_without_descending_into_them() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("vendor/Thing.vst3/Contents/x86_64-linux");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("Thing.so"), b"").unwrap();
        let found = find_candidates(PluginFormat::Vst3, &[dir.path().to_path_buf()]);
        assert_eq!(found, vec![dir.path().join("vendor/Thing.vst3")]);
    }
}
