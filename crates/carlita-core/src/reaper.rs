//! Importing REAPER projects (`.rpp`) as presets.
//!
//! Only what Carlita can play comes across: each track with plugins becomes a
//! track with the same name, volume, mute and input, and its FX chain becomes
//! the track's chain (bypass included; FX that REAPER runs in parallel with
//! the previous FX become lanes of a parallel block). Each plugin's settings
//! are turned into the Carla state Carlita keeps in presets:
//!
//! * VST3: the plugin's own state block, as Carla stores it.
//! * VST2: the plugin's chunk (plugins that save their state as a chunk).
//! * LV2: control port values (by symbol) and the plugin's string, path and
//!   integer state properties.
//!
//! Plugins are found in the scanned catalog by their ids (VST3 class id, VST2
//! unique id, LV2 URI, CLAP id), falling back to the binary's file name.
//! Tracks without plugins, folders, sends, items, envelopes and the master
//! track are left out. Nothing is ever written to the project.

use crate::chain::ChainSlot;
use crate::plugin::{PluginDescriptor, PluginFormat};
use crate::preset::Preset;
use crate::rig::{Block, Lane, Parallel, Rig, TrackInput, TrackMidi};
use anyhow::{bail, Context};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// A project turned into a preset, and what could not come across.
#[derive(Debug, Clone)]
pub struct Import {
    pub preset: Preset,
    pub report: ImportReport,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    /// Tracks and plugins brought in.
    pub tracks: usize,
    pub plugins: usize,
    /// Plugins the project uses that are not installed (or not scanned).
    pub missing: Vec<String>,
    /// FX Carlita cannot host (JSFX, containers, video processors, ...).
    pub unsupported: Vec<String>,
    /// Plugins brought in with their default settings.
    pub defaults: Vec<String>,
}

impl ImportReport {
    /// One line per kind of problem, empty when everything came across.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Each name once, in the order met.
        let list = |v: &[String]| {
            let mut seen: Vec<&str> = Vec::new();
            for n in v {
                if !seen.contains(&n.as_str()) {
                    seen.push(n);
                }
            }
            seen.join(", ")
        };
        if !self.missing.is_empty() {
            out.push(format!("Not installed: {}", list(&self.missing)));
        }
        if !self.unsupported.is_empty() {
            out.push(format!("Not supported: {}", list(&self.unsupported)));
        }
        if !self.defaults.is_empty() {
            out.push(format!("Settings not imported: {}", list(&self.defaults)));
        }
        out
    }

    /// "3 tracks, 7 plugins".
    pub fn counts(&self) -> String {
        let s = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        format!("{}, {}", s(self.tracks, "track"), s(self.plugins, "plugin"))
    }
}

/// Reads a project and turns it into a preset named after the file.
pub fn import_file(path: &Path, catalog: &[PluginDescriptor]) -> anyhow::Result<Import> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "REAPER project".into());
    import_str(&text, &name, catalog)
}

/// Turns the text of a project into a preset called `name`.
pub fn import_str(text: &str, name: &str, catalog: &[PluginDescriptor]) -> anyhow::Result<Import> {
    let root = parse(text)?;
    if root.tokens.first().map(String::as_str) != Some("REAPER_PROJECT") {
        bail!("not a REAPER project");
    }
    let mut report = ImportReport::default();
    let mut rig = Rig::default();
    for (n, track) in root.nodes("TRACK").enumerate() {
        import_track(&mut rig, track, n, catalog, &mut report);
    }
    report.tracks = rig.tracks.len();
    report.plugins = rig.slot_count();
    rig.ensure_track();
    Ok(Import {
        preset: Preset::new(name.trim(), rig, Vec::new()),
        report,
    })
}

/// REAPER projects in `dir` and the folders below it (a few levels deep,
/// hidden folders skipped, a bounded number of folders looked at), sorted by
/// path.
pub fn find_projects(dir: &Path) -> Vec<PathBuf> {
    struct Walk {
        out: Vec<PathBuf>,
        folders: usize,
    }
    fn walk(dir: &Path, depth: u32, w: &mut Walk) {
        w.folders += 1;
        if w.folders > 2000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if w.out.len() >= 500 {
                return;
            }
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                if depth > 0 {
                    walk(&path, depth - 1, w);
                }
            } else if path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("rpp"))
            {
                w.out.push(path);
            }
        }
    }
    let mut w = Walk {
        out: Vec::new(),
        folders: 0,
    };
    walk(dir, 3, &mut w);
    w.out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    w.out
}

// ================================================================= tracks

fn import_track(
    rig: &mut Rig,
    track: &Node,
    index: usize,
    catalog: &[PluginDescriptor],
    report: &mut ImportReport,
) {
    let Some(chain) = track.node("FXCHAIN") else {
        return;
    };
    let fx = chain_fx(chain);
    if fx.is_empty() {
        return;
    }
    // The track's FX button bypasses its whole chain.
    let chain_off = track.value("FX", 1).as_deref() == Some("0");

    // Group the FX: one that runs in parallel with the previous one joins
    // its group as another lane.
    let mut groups: Vec<Vec<ChainSlot>> = Vec::new();
    let mut prev_group: Option<usize> = None;
    for f in &fx {
        let slot = match resolve(f, catalog, report) {
            Some((desc, name, state)) => {
                let mut slot = rig.new_slot(desc);
                if let Some(name) = name {
                    slot.name = name;
                }
                slot.state = state;
                slot.bypassed = f.bypass || f.offline || chain_off;
                Some(slot)
            }
            None => None,
        };
        let g = match prev_group {
            Some(g) if f.parallel => g,
            _ => {
                groups.push(Vec::new());
                groups.len() - 1
            }
        };
        prev_group = Some(g);
        groups[g].extend(slot);
    }
    let blocks: Vec<Block> = groups
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|mut g| {
            if g.len() == 1 {
                Block::Plugin(g.remove(0))
            } else {
                Block::Parallel(Parallel {
                    lanes: g.into_iter().map(|s| Lane { slots: vec![s] }).collect(),
                })
            }
        })
        .collect();
    if blocks.is_empty() {
        return;
    }

    let name = track
        .value("NAME", 1)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Track {}", index + 1));
    let id = rig.add_track(Some(name));
    let t = rig.track_mut(id).expect("just added");
    t.blocks = blocks;
    t.volume = track
        .value("VOLPAN", 1)
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .map_or(1.0, |v| v.clamp(0.0, 4.0));
    t.muted = track.value("MUTESOLO", 1).is_some_and(|v| v != "0");
    t.input = track
        .value("REC", 2)
        .and_then(|v| v.parse::<i64>().ok())
        .map_or_else(TrackInput::none, record_input);
    // REAPER numbers MIDI devices by its own list, which Carlita cannot see;
    // every track takes every device, as new tracks do.
    t.midi = TrackMidi::All;
}

/// The hardware inputs of a track's record input (`I_RECINPUT`).
fn record_input(rec: i64) -> TrackInput {
    const REAROUTE: i64 = 512;
    const STEREO: i64 = 1024;
    const MULTI: i64 = 2048;
    const MIDI: i64 = 4096;
    if rec < 0 || rec & (MIDI | REAROUTE) != 0 {
        return TrackInput::none();
    }
    let first = (rec & 511) as u16;
    if rec & (STEREO | MULTI) != 0 {
        TrackInput::new([first, first + 1])
    } else {
        TrackInput::new([first])
    }
}

// ===================================================================== FX

struct Fx<'a> {
    node: &'a Node,
    bypass: bool,
    offline: bool,
    /// Runs in parallel with the FX before it.
    parallel: bool,
}

/// The FX of a chain, in order. A `BYPASS` line comes before each FX and a
/// `PARALLEL` line (REAPER 7) after it.
fn chain_fx(chain: &Node) -> Vec<Fx<'_>> {
    let mut out: Vec<Fx> = Vec::new();
    let mut bypass = (false, false);
    for item in &chain.items {
        match item {
            Item::Line(l) => {
                let t = tokenize(l);
                match t.first().map(String::as_str) {
                    Some("BYPASS") => {
                        bypass = (
                            t.get(1).is_some_and(|v| v != "0"),
                            t.get(2).is_some_and(|v| v != "0"),
                        );
                    }
                    Some("PARALLEL") => {
                        if let Some(last) = out.last_mut() {
                            last.parallel = t.get(1).is_some_and(|v| v != "0");
                        }
                    }
                    _ => {}
                }
            }
            Item::Node(n) => {
                out.push(Fx {
                    node: n,
                    bypass: bypass.0,
                    offline: bypass.1,
                    parallel: false,
                });
                bypass = (false, false);
            }
        }
    }
    out
}

/// "VST3i: Vital (Vital Audio)" without the format prefix and output count.
fn display_name(full: &str) -> String {
    let s = full.split_once(": ").map_or(full, |(_, s)| s).trim();
    // "(32 out)" etc.
    let s = match s.rfind(" (") {
        Some(i) if s[i..].ends_with(" out)") => &s[..i],
        _ => s,
    };
    s.to_string()
}

/// The catalog plugin for an FX, the name to show (when renamed in REAPER)
/// and its Carla state.
fn resolve(
    fx: &Fx,
    catalog: &[PluginDescriptor],
    report: &mut ImportReport,
) -> Option<(PluginDescriptor, Option<String>, Option<String>)> {
    let n = fx.node;
    let kind = n.tokens.first().map(String::as_str).unwrap_or("");
    let tok = |i: usize| n.tokens.get(i).map(String::as_str).unwrap_or("");
    let shown = display_name(tok(1));
    let found = match kind {
        "VST" => {
            let vst3 = tok(1).starts_with("VST3");
            let file = tok(2);
            let custom = tok(4);
            let id = tok(5);
            let desc = if vst3 {
                find_vst3(catalog, id, file, &shown)
            } else {
                find_vst2(catalog, id, file, &shown)
            };
            desc.map(|d| {
                let state = vst_state(n, vst3).map(|chunk| carxs(&d, &[], &[], Some(&chunk)));
                (d, custom, state)
            })
        }
        "LV2" => {
            let uri = tok(2);
            catalog
                .iter()
                .find(|p| p.format == PluginFormat::Lv2 && p.label == uri)
                .cloned()
                .map(|d| {
                    let (params, custom) = lv2_state(n);
                    let state = (!params.is_empty() || !custom.is_empty())
                        .then(|| carxs(&d, &params, &custom, None));
                    (d, tok(3), state)
                })
        }
        "CLAP" => {
            let id = tok(2);
            let d = catalog
                .iter()
                .find(|p| p.format == PluginFormat::Clap && p.label == id)
                .cloned();
            if d.is_some() {
                report.defaults.push(shown.clone());
            }
            d.map(|d| (d, tok(3), None))
        }
        _ => {
            let what = match kind {
                "JS" => format!("JSFX {}", tok(1)),
                "CONTAINER" => "FX container".into(),
                "VIDEO_EFFECT" => "video processor".into(),
                "DX" => format!("DirectX {shown}"),
                "AU" => format!("AU {shown}"),
                other => other.to_lowercase(),
            };
            report.unsupported.push(what);
            return None;
        }
    };
    let Some((desc, custom, state)) = found else {
        report.missing.push(shown);
        return None;
    };
    let name = Some(custom.trim())
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    if state.is_none() && desc.format != PluginFormat::Clap && !n.items.is_empty() {
        report.defaults.push(shown);
    }
    Some((desc, name, state))
}

/// VST3 class id as Carla lists it ("0x56535448,0x54323468,..."), from
/// REAPER's "1356796942{56535448543234687970657274756E65}".
fn vst3_label(id: &str) -> Option<String> {
    let hex = id.split_once('{')?.1.split_once('}')?.0;
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_uppercase();
    Some(
        (0..4)
            .map(|i| format!("0x{}", &hex[i * 8..i * 8 + 8]))
            .collect::<Vec<_>>()
            .join(","),
    )
}

fn file_stem(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    base.split('.').next().unwrap_or(base).to_lowercase()
}

/// Several plugins share a bundle (or a unique id): prefer the one named as
/// in the project.
fn pick<'a>(
    mut candidates: Vec<&'a PluginDescriptor>,
    shown: &str,
) -> Option<&'a PluginDescriptor> {
    if candidates.len() > 1 {
        let shown = shown.to_lowercase();
        if let Some(i) = candidates
            .iter()
            .position(|p| shown.starts_with(&p.name.to_lowercase()))
        {
            return Some(candidates.swap_remove(i));
        }
    }
    candidates.into_iter().next()
}

fn find_vst3(
    catalog: &[PluginDescriptor],
    id: &str,
    file: &str,
    shown: &str,
) -> Option<PluginDescriptor> {
    let vst3 = || catalog.iter().filter(|p| p.format == PluginFormat::Vst3);
    if let Some(label) = vst3_label(id) {
        if let Some(p) = vst3().find(|p| p.label.eq_ignore_ascii_case(&label)) {
            return Some(p.clone());
        }
    }
    let stem = file_stem(file);
    pick(
        vst3().filter(|p| file_stem(&p.filename) == stem).collect(),
        shown,
    )
    .cloned()
}

fn find_vst2(
    catalog: &[PluginDescriptor],
    id: &str,
    file: &str,
    shown: &str,
) -> Option<PluginDescriptor> {
    let vst2 = || catalog.iter().filter(|p| p.format == PluginFormat::Vst2);
    let stem = file_stem(file);
    let uid: Option<i64> = id
        .split(['<', '{'])
        .next()
        .and_then(|s| s.trim().parse().ok());
    if let Some(uid) = uid.filter(|u| *u != 0) {
        let same: Vec<_> = vst2().filter(|p| p.unique_id == uid).collect();
        if let Some(p) = same.iter().find(|p| file_stem(&p.filename) == stem) {
            return Some((*p).clone());
        }
        if let Some(p) = pick(same, shown) {
            return Some(p.clone());
        }
    }
    pick(
        vst2().filter(|p| file_stem(&p.filename) == stem).collect(),
        shown,
    )
    .cloned()
}

// ================================================================== state

/// The plugin data REAPER keeps for a VST: a header (unique id, magic, pin
/// mappings, data size) and the data. For VST3 the data holds the
/// component's state, which is what Carla stores; for VST2 it is the
/// plugin's chunk.
fn vst_state(node: &Node, vst3: bool) -> Option<Vec<u8>> {
    // Each line is base64 on its own.
    let mut bytes = Vec::new();
    for item in &node.items {
        if let Item::Line(l) = item {
            bytes.extend(base64_decode(l.trim())?);
        }
    }
    let mut r = Reader(&bytes);
    r.u32()?; // unique id
    let magic = r.u32()?;
    if magic & 0xFFFF_FFF0 != 0xFEED_5EE0 {
        return None;
    }
    for _ in 0..2 {
        let pins = usize::try_from(r.i32()?).ok()?;
        r.take(pins.checked_mul(8)?)?;
    }
    let size = r.u32()? as usize;
    r.take(8)?;
    let data = r.take(size)?;
    if !vst3 {
        return (!data.is_empty()).then(|| data.to_vec());
    }
    let mut d = Reader(data);
    let component = d.u32()? as usize;
    d.take(4)?;
    let state = d.take(component)?;
    (!state.is_empty()).then(|| state.to_vec())
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(self.u32()? as i32)
    }
}

const ATOM_STRING: &str = "http://lv2plug.in/ns/ext/atom#String";
const ATOM_PATH: &str = "http://lv2plug.in/ns/ext/atom#Path";
const ATOM_INT: &str = "http://lv2plug.in/ns/ext/atom#Int";

/// Control port values (`P symbol value ...` lines) and state properties
/// (`<STATE`: `S`tring, `P`ath and `I`nt; others are left out).
#[allow(clippy::type_complexity)]
fn lv2_state(node: &Node) -> (Vec<(String, String)>, Vec<(String, String, String)>) {
    let mut params = Vec::new();
    for item in &node.items {
        let Item::Line(l) = item else { continue };
        let t = tokenize(l);
        if t.first().map(String::as_str) != Some("P") {
            continue;
        }
        for pair in t[1..].chunks_exact(2) {
            if pair[1].parse::<f64>().is_ok_and(f64::is_finite) {
                params.push((pair[0].clone(), pair[1].clone()));
            }
        }
    }
    let mut custom = Vec::new();
    if let Some(state) = node.node("STATE") {
        for item in &state.items {
            let (kind, key, value) = match item {
                Item::Line(l) => {
                    let t = tokenize(l);
                    if t.len() < 3 {
                        continue;
                    }
                    (t[0].clone(), t[1].clone(), t[2].clone())
                }
                // A long string, one `|` line per line of text, `c|` lines
                // carrying on the line before.
                Item::Node(n) => {
                    let mut value = String::new();
                    for (i, item) in n.items.iter().enumerate() {
                        let Item::Line(l) = item else { continue };
                        if let Some(rest) = l.strip_prefix("c|") {
                            value.push_str(rest);
                        } else if let Some(rest) = l.strip_prefix('|') {
                            if i > 0 {
                                value.push('\n');
                            }
                            value.push_str(rest);
                        }
                    }
                    let tok = |i: usize| n.tokens.get(i).cloned().unwrap_or_default();
                    (tok(0), tok(1), value)
                }
            };
            if key.is_empty() || value.is_empty() {
                continue;
            }
            match kind.as_str() {
                "S" => custom.push((ATOM_STRING.into(), key, value)),
                "P" => custom.push((ATOM_PATH.into(), key, value)),
                "I" => {
                    if let Ok(v) = value.parse::<i32>() {
                        custom.push((ATOM_INT.into(), key, base64_encode(&v.to_le_bytes())));
                    }
                }
                _ => {}
            }
        }
    }
    (params, custom)
}

/// Carla's plugin state file (`.carxs`), as `carla_load_plugin_state`
/// reads it. `<Options>` is left out on purpose: the engine fills in the
/// plugin's own options before loading, since Carla applies them from the
/// file.
fn carxs(
    d: &PluginDescriptor,
    params: &[(String, String)],
    custom: &[(String, String, String)],
    chunk: Option<&[u8]>,
) -> String {
    let mut x = String::new();
    x.push_str("<?xml version='1.0' encoding='UTF-8'?>\n<!DOCTYPE CARLA-PRESET>\n");
    x.push_str("<CARLA-PRESET VERSION='2.0'>\n  <Info>\n");
    let (ty, extra) = match d.format {
        PluginFormat::Lv2 => ("LV2", vec![("URI", d.label.clone())]),
        PluginFormat::Vst2 => (
            "VST2",
            vec![
                ("Binary", d.filename.clone()),
                ("UniqueID", d.unique_id.to_string()),
            ],
        ),
        PluginFormat::Vst3 => (
            "VST3",
            vec![("Binary", d.filename.clone()), ("Label", d.label.clone())],
        ),
        PluginFormat::Clap => (
            "CLAP",
            vec![
                ("Binary", d.filename.clone()),
                ("Identifier", d.label.clone()),
            ],
        ),
        PluginFormat::Ladspa => ("LADSPA", vec![("Label", d.label.clone())]),
        PluginFormat::Internal => ("INTERNAL", vec![("Label", d.label.clone())]),
    };
    let _ = writeln!(x, "   <Type>{ty}</Type>");
    let _ = writeln!(x, "   <Name>{}</Name>", xml(&d.name));
    for (tag, v) in extra {
        let _ = writeln!(x, "   <{tag}>{}</{tag}>", xml(&v));
    }
    x.push_str("  </Info>\n\n  <Data>\n");
    x.push_str("   <Active>Yes</Active>\n   <ControlChannel>1</ControlChannel>\n");
    for (i, (symbol, value)) in params.iter().enumerate() {
        let _ = write!(
            x,
            "\n   <Parameter>\n    <Index>{i}</Index>\n    <Name>{s}</Name>\n    \
             <Symbol>{s}</Symbol>\n    <Value>{value}</Value>\n   </Parameter>\n",
            s = xml(symbol)
        );
    }
    for (ty, key, value) in custom {
        let _ = write!(
            x,
            "\n   <CustomData>\n    <Type>{}</Type>\n    <Key>{}</Key>\n",
            xml(ty),
            xml(key)
        );
        // Carla reads a value of 128 bytes or more as wrapped in a newline
        // before and a newline and four spaces after.
        if value.len() >= 128 {
            let _ = writeln!(x, "    <Value>\n{}\n    </Value>", xml(value));
        } else {
            let _ = writeln!(x, "    <Value>{}</Value>", xml(value));
        }
        x.push_str("   </CustomData>\n");
    }
    if let Some(chunk) = chunk {
        let _ = write!(x, "\n   <Chunk>\n{}\n   </Chunk>\n", base64_encode(chunk));
    }
    x.push_str("  </Data>\n</CARLA-PRESET>\n");
    x
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ================================================================= parser

/// A `<TAG ...>` block of a project.
#[derive(Debug, Default)]
struct Node {
    tokens: Vec<String>,
    items: Vec<Item>,
}

#[derive(Debug)]
enum Item {
    /// A line inside the block, without its indentation.
    Line(String),
    Node(Node),
}

impl Node {
    fn nodes<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.items.iter().filter_map(move |i| match i {
            Item::Node(n) if n.tokens.first().map(String::as_str) == Some(tag) => Some(n),
            _ => None,
        })
    }

    fn node(&self, tag: &str) -> Option<&Node> {
        self.items.iter().find_map(|i| match i {
            Item::Node(n) if n.tokens.first().map(String::as_str) == Some(tag) => Some(n),
            _ => None,
        })
    }

    /// Token `i` of the first line starting with `key`.
    fn value(&self, key: &str, i: usize) -> Option<String> {
        self.items.iter().find_map(|item| match item {
            Item::Line(l) if l.split_whitespace().next() == Some(key) => {
                tokenize(l).into_iter().nth(i)
            }
            _ => None,
        })
    }
}

fn parse(text: &str) -> anyhow::Result<Node> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    for line in text.lines() {
        let l = line.trim_start().trim_end_matches('\r');
        if let Some(head) = l.strip_prefix('<') {
            stack.push(Node {
                tokens: tokenize(head),
                items: Vec::new(),
            });
        } else if l.trim_end() == ">" {
            let Some(done) = stack.pop() else {
                bail!("unbalanced project file");
            };
            match stack.last_mut() {
                Some(parent) => parent.items.push(Item::Node(done)),
                None => {
                    root = Some(done);
                    break;
                }
            }
        } else if let Some(n) = stack.last_mut() {
            if !l.is_empty() {
                n.items.push(Item::Line(l.to_string()));
            }
        }
    }
    // A file cut short: close what is open.
    while let Some(done) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.items.push(Item::Node(done)),
            None => root = Some(done),
        }
    }
    root.context("empty project file")
}

/// Splits a line into tokens. REAPER quotes a token holding spaces with
/// whichever of `"`, `'` or `` ` `` it does not contain.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        let mut tok = String::new();
        if matches!(c, '"' | '\'' | '`') {
            chars.next();
            for ch in chars.by_ref() {
                if ch == c {
                    break;
                }
                tok.push(ch);
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                tok.push(ch);
                chars.next();
            }
        }
        out.push(tok);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginCategory;

    fn desc(format: PluginFormat, name: &str, label: &str, filename: &str) -> PluginDescriptor {
        PluginDescriptor {
            format,
            arch: Default::default(),
            name: name.into(),
            label: label.into(),
            maker: String::new(),
            filename: filename.into(),
            unique_id: 0,
            category: PluginCategory::Other,
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

    fn catalog() -> Vec<PluginDescriptor> {
        vec![
            desc(
                PluginFormat::Vst3,
                "Modometer",
                "0x4D494449,0x4B616961,0x436F756E,0x74657231",
                "/usr/lib/vst3/Modometer.vst3",
            ),
            desc(
                PluginFormat::Vst3,
                "HyperTune",
                "something else",
                "/home/u/.vst3/HyperTune.vst3",
            ),
            desc(
                PluginFormat::Lv2,
                "Compressor",
                "http://example.org/comp",
                "",
            ),
            desc(PluginFormat::Lv2, "Synth", "http://example.org/synth", ""),
        ]
    }

    /// Modometer's state as REAPER saved it (2 ins, 2 outs).
    const MODOMETER: &str = concat!(
        "qxDVEO5e7f4CAAAAAQAAAAAAAAACAAAAAAAAAAIAAAABAAAAAAAAAAIAAAAAAAAAOAAAAAEAAAD//wAA\n",
        "        KAAAAAEAAABNT0RPTUVURVIxCkFsZXNpcyBDb250cm9sUGFkCnpvb20gMS40MjcKAAAAAAAAAAA=\n",
        "        AAAAAAAA"
    );

    fn project() -> String {
        format!(
            r#"<REAPER_PROJECT 0.1 "7.84/linux-x86_64" 1793717047 0
  <NOTES 0 2
  >
  TEMPO 120 4 4 0
  <TRACK {{A}}
    NAME "Pads"
    VOLPAN 0.5 0 -1 -1 1
    MUTESOLO 1 0 0
    REC 1 6624 1 0 0 0 0 0
    FX 1
    <FXCHAIN
      SHOW 0
      BYPASS 0 0 0
      <VST "VST3: Modometer (Kaia)" Modometer.vst3 0 "" 282398891{{4D4944494B616961436F756E74657231}} ""
        {MODOMETER}
      >
      FXID {{X}}
      BYPASS 1 0 0
      <LV2 "LV2i: Synth (Example) (34 out)" http://example.org/synth "My synth"
        P enabled 1
        <STATE
          <S http://example.org/synth:State 3
             |first pa
            c|rt and the rest
             |second line
          >
        >
      >
      BYPASS 0 0 0
      <JS loser/3BandEQ ""
        0 200 0 2000 0 0 - - -
      >
      PARALLEL 1
      BYPASS 0 0 0
      <LV2 "LV2: Compressor (Example)" http://example.org/comp ""
        P g_in 1 g_out 0.266809999942779541 bogus nan
        <STATE
          S http://example.org/comp/ports#link "" 3
          P http://example.org/comp/ports#ifn "/home/u/IR's & <stuff>.wav" 3
          I http://example.org/comp#flags 7 3
          <L http://example.org/comp/preset_state "" http://example.org/PresetState 3
            S http://example.org/PresetState#name Smush 3
          >
        >
      >
      PARALLEL 2
      BYPASS 0 0 0
      <VST "VST3: Not Here (Nobody)" NotHere.vst3 0 "" 1{{00000000000000000000000000000001}} ""
        AAAA
      >
    >
  >
  <TRACK {{B}}
    NAME "Empty"
    REC 0 3 1 0 0 0 0 0
    <FXCHAIN
      SHOW 0
    >
  >
  <TRACK {{C}}
    NAME ""
    REC 1 1026 1 0 0 0 0 0
    FX 0
    <FXCHAIN
      BYPASS 0 0 0
      <VST "VST3: HyperTune (PolyChrome DSP)" HyperTune.vst3 0 "" 1{{11111111111111111111111111111111}} ""
        {MODOMETER}
      >
    >
  >
>
"#
        )
    }

    #[test]
    fn imports_tracks_chains_and_state() {
        let imp = import_str(&project(), "Live", &catalog()).unwrap();
        let rig = &imp.preset.rig;
        assert_eq!(imp.preset.name, "Live");
        // The track without plugins is left out; the unnamed one is named
        // after its position.
        let names: Vec<_> = rig.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Pads", "Track 3"]);

        let pads = &rig.tracks[0];
        assert_eq!(pads.volume, 0.5);
        assert!(pads.muted);
        assert!(pads.input.is_empty(), "MIDI input means no audio input");
        assert_eq!(pads.midi, TrackMidi::All);
        // Modometer, then the synth and the compressor in parallel (the JS
        // effect between them is left out but keeps them in one group).
        assert_eq!(pads.blocks.len(), 2);
        let Block::Plugin(modo) = &pads.blocks[0] else {
            panic!("serial first")
        };
        assert_eq!(modo.name, "Modometer");
        assert!(!modo.bypassed);
        let state = modo.state.as_deref().unwrap();
        // The component state alone ("MODOMETER1\nAlesis ControlPad\n...").
        let chunk = state
            .split("<Chunk>\n")
            .nth(1)
            .unwrap()
            .split('\n')
            .next()
            .unwrap();
        let bytes = base64_decode(chunk).unwrap();
        assert_eq!(bytes.len(), 40);
        assert!(bytes.starts_with(b"MODOMETER1\nAlesis ControlPad\n"));
        assert!(state.contains("<Type>VST3</Type>"));
        assert!(!state.contains("<Options>"));

        let Block::Parallel(p) = &pads.blocks[1] else {
            panic!("parallel second")
        };
        assert_eq!(p.lanes.len(), 2);
        let synth = &p.lanes[0].slots[0];
        assert_eq!(synth.name, "My synth");
        assert!(synth.bypassed);
        let s = synth.state.as_deref().unwrap();
        assert!(s.contains("<Symbol>enabled</Symbol>\n    <Value>1</Value>"));
        assert!(s.contains("<Key>http://example.org/synth:State</Key>\n    <Value>first part and the rest\nsecond line</Value>"));

        let comp = &p.lanes[1].slots[0];
        let s = comp.state.as_deref().unwrap();
        assert!(s.contains("<Symbol>g_out</Symbol>\n    <Value>0.266809999942779541</Value>"));
        assert!(!s.contains("bogus"));
        assert!(!s.contains("ports#link"), "empty values are left out");
        assert!(!s.contains("PresetState"), "objects are left out");
        assert!(s.contains("<Type>http://lv2plug.in/ns/ext/atom#Path</Type>"));
        assert!(s.contains("/home/u/IR&apos;s &amp; &lt;stuff&gt;.wav"));
        assert!(s.contains("<Type>http://lv2plug.in/ns/ext/atom#Int</Type>\n    <Key>http://example.org/comp#flags</Key>\n    <Value>BwAAAA==</Value>"));

        // Found by bundle name; the track's FX button was off; a stereo input
        // from input 3.
        let t = &rig.tracks[1];
        let Block::Plugin(ht) = &t.blocks[0] else {
            panic!()
        };
        assert_eq!(ht.plugin.name, "HyperTune");
        assert!(ht.bypassed);
        assert_eq!(t.input.channels(), [2, 3]);

        assert_eq!(imp.report.tracks, 2);
        assert_eq!(imp.report.plugins, 4);
        assert_eq!(imp.report.missing, ["Not Here (Nobody)"]);
        assert_eq!(imp.report.unsupported, ["JSFX loser/3BandEQ"]);
        assert!(imp.report.defaults.is_empty());
        let mut twice = imp.report.clone();
        twice.missing.push("Other".into());
        twice.missing.push("Not Here (Nobody)".into());
        assert_eq!(
            twice.problems()[0],
            "Not installed: Not Here (Nobody), Other"
        );
        // Slot ids are unique.
        let mut ids: Vec<_> = rig.slots().map(|s| s.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn record_inputs() {
        assert!(record_input(-1).is_empty());
        assert_eq!(record_input(0).channels(), [0]);
        assert_eq!(record_input(7).channels(), [7]);
        assert_eq!(record_input(1024).channels(), [0, 1]);
        assert!(record_input(6208).is_empty());
    }

    #[test]
    fn tokens_and_names() {
        assert_eq!(
            tokenize(r#"<VST "VST3: A (B)" 'x "y"' `q 'r'` 12{AB} """#.trim_start_matches('<')),
            ["VST", "VST3: A (B)", "x \"y\"", "q 'r'", "12{AB}", ""]
        );
        assert_eq!(
            display_name("VST3i: XO (XLN Audio) (18 out)"),
            "XO (XLN Audio)"
        );
        assert_eq!(display_name("LV2: Xythos (Xythos)"), "Xythos (Xythos)");
        assert_eq!(
            vst3_label("1{4D4944494B616961436F756E74657231}").unwrap(),
            "0x4D494449,0x4B616961,0x436F756E,0x74657231"
        );
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 5) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn rejects_other_files() {
        assert!(import_str("hello\n", "x", &[]).is_err());
        assert!(import_str("<SOMETHING\n>\n", "x", &[]).is_err());
    }

    #[test]
    fn finds_projects() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Songs/Live");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        for f in [
            "a.RPP",
            "Songs/Live/b.rpp",
            "Songs/Live/b.rpp-bak",
            ".hidden/c.rpp",
        ] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let found = find_projects(dir.path());
        let rel: Vec<_> = found
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(rel, ["a.RPP", "Songs/Live/b.rpp"]);
    }
}
