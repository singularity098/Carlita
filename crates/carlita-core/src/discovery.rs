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
        yabridge: is_yabridge_path(filename),
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
    /// The reason, and the last lines the probe (and what it started, such
    /// as yabridge and Wine) printed.
    Failed(String, String),
    TimedOut(String),
}

/// How many of the last lines of a probe's error output are kept.
const ERROR_LINES: usize = 12;

/// Whether `target` is a Windows plugin run through yabridge. Scanning one
/// starts Wine, which can take far longer than a native plugin.
pub fn is_yabridge_path(target: &str) -> bool {
    target.contains("/yabridge/") || target.contains("/yabridge-")
}

/// How long to wait for the rest of the output once the discovery tool has
/// exited. A process it started (Wine, a yabridge host) can keep the pipe
/// open long after.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `carla-discovery-native <format> <target>` with a timeout.
pub fn probe(
    discovery_tool: &Path,
    format: PluginFormat,
    target: &str,
    timeout: Duration,
) -> ProbeResult {
    use std::os::unix::process::CommandExt;
    let child = Command::new(discovery_tool)
        .arg(format.discovery_arg())
        .arg(target)
        // Only read the plugin's description, as Carla's own scanner does
        // by default: the processing test instantiates and runs the plugin,
        // which is slow and fragile for yabridge (Wine) plugins and makes
        // them fail or time out.
        .env("CARLA_DISCOVERY_NO_PROCESSING_CHECKS", "true")
        // A group of its own, so a hung probe can be stopped together with
        // whatever it started (Wine, yabridge's host).
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult::Failed(
                format!("cannot run {}: {e}", discovery_tool.display()),
                String::new(),
            )
        }
    };

    // The error output (yabridge and Wine report there), for the scan log.
    // Only its last lines are kept.
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    {
        let errors = errors.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut partial = Vec::new();
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        partial.extend_from_slice(&buf[..n]);
                        while let Some(i) = partial.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = partial.drain(..=i).collect();
                            let line = String::from_utf8_lossy(&line).trim_end().to_string();
                            if line.is_empty() {
                                continue;
                            }
                            let mut e = errors.lock().unwrap();
                            if e.len() == ERROR_LINES {
                                e.pop_front();
                            }
                            e.push_back(line);
                        }
                    }
                }
            }
        });
    }
    let error_text = move || {
        errors
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Read stdout on a helper thread so a chatty plugin cannot fill the pipe
    // and deadlock while we wait for it. It is passed on in pieces, so the
    // output is not lost when a process the probe started keeps the pipe
    // open after the probe has exited.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut output = Vec::new();
    let start = Instant::now();
    loop {
        while let Ok(chunk) = rx.try_recv() {
            output.extend(chunk);
        }
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if start.elapsed() > timeout => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return ProbeResult::TimedOut(error_text());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return ProbeResult::Failed(e.to_string(), error_text()),
        }
    }
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => output.extend(chunk),
            Err(_) => break,
        }
    }
    let output = String::from_utf8_lossy(&output);
    let filename = if target.starts_with(':') { "" } else { target };
    let plugins = parse_discovery_output(format, filename, &output);
    if plugins.is_empty() {
        let err = output
            .lines()
            .find_map(|l| l.trim().strip_prefix("carla-discovery::error::"))
            .unwrap_or("no plugins found");
        ProbeResult::Failed(err.to_string(), error_text())
    } else {
        ProbeResult::Ok(plugins)
    }
}

/// Kills every process in the group led by `pid`.
fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
    /// A readable account of the scan: the folders searched, and what each
    /// probe found or why it failed (with its error output).
    pub log: String,
}

/// Scans all formats. `progress` is called before each probe.
pub fn scan_all(
    discovery_tool: &Path,
    formats: &[PluginFormat],
    timeout: Duration,
    mut progress: impl FnMut(ScanProgress),
) -> ScanReport {
    use std::fmt::Write;
    let mut report = ScanReport::default();
    let log = &mut report.log;
    let _ = writeln!(log, "Scanner: {}", discovery_tool.display());
    let mut jobs: Vec<(PluginFormat, String)> = Vec::new();
    for &format in formats {
        match format {
            PluginFormat::Lv2 | PluginFormat::Internal => jobs.push((format, ":all".into())),
            _ => {
                let roots = default_search_paths(format);
                let found = find_candidates(format, &roots);
                let _ = writeln!(log, "\n{}: {} found", format.label(), found.len());
                for r in &roots {
                    let n = found.iter().filter(|c| c.starts_with(r)).count();
                    let state = if r.is_dir() {
                        format!("{n}")
                    } else {
                        "missing".into()
                    };
                    let _ = writeln!(log, "  {:>7}  {}", state, r.display());
                }
                for c in found {
                    jobs.push((format, c.to_string_lossy().into_owned()));
                }
            }
        }
    }
    let yabridge = jobs.iter().filter(|(_, t)| is_yabridge_path(t)).count();
    let _ = writeln!(log, "\nyabridge plugins found: {yabridge}\n");

    let total = jobs.len();
    for (i, (format, target)) in jobs.into_iter().enumerate() {
        progress(ScanProgress {
            done: i,
            total,
            current: target.clone(),
        });
        // LV2 ":all" scans everything at once, and a yabridge plugin starts
        // Wine first (the first one can take very long), so give them longer.
        let t = if target.starts_with(':') || is_yabridge_path(&target) {
            timeout * 6
        } else {
            timeout
        };
        let started = Instant::now();
        let result = probe(discovery_tool, format, &target, t);
        let secs = started.elapsed().as_secs_f32();
        let (outcome, errors) = match &result {
            ProbeResult::Ok(p) => (format!("ok, {} plugin(s)", p.len()), ""),
            ProbeResult::Failed(e, out) => (format!("FAILED: {e}"), out.as_str()),
            ProbeResult::TimedOut(out) => {
                (format!("TIMED OUT after {}s", t.as_secs()), out.as_str())
            }
        };
        let log = &mut report.log;
        let _ = writeln!(log, "[{}] {target}: {outcome} ({secs:.1}s)", format.label());
        // The error output of failed probes only: plugins that load fine
        // can be chatty.
        if !errors.is_empty() {
            for l in errors.lines() {
                let _ = writeln!(log, "      | {l}");
            }
        }
        match result {
            ProbeResult::Ok(p) => report.plugins.extend(p),
            ProbeResult::Failed(e, _) => report.failures.push((target, e)),
            ProbeResult::TimedOut(_) => report.failures.push((target, "timed out".into())),
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
