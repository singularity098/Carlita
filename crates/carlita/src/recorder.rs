//! Stem recording (JACK only): Carlita's own JACK client, with an input
//! for each channel of each track's output (and, if asked, of the hardware
//! inputs the track takes), writes one file per track and take (WAV or
//! FLAC, see `stem_file`).
//!
//! The engine wires the client up (`Command::SetRecorder`); its ports are
//! named by `carlita_engine::recorder_port`. All ports belong to this one
//! client, so JACK hands them over in the same cycle and the stems line up
//! to the sample. The audio thread only copies samples into ring buffers;
//! a writer thread writes the files.

use crate::stem_file::{self, StemFile};
use anyhow::{bail, Context, Result};
use carlita_core::settings::RecordFormat;
use carlita_core::TrackId;
use carlita_engine::recorder_port;
use jack::{AudioIn, Client, ClientOptions, Control, Port, ProcessScope};
use rtrb::{Consumer, Producer, RingBuffer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The recorder's JACK client name. "Carlita." keeps it out of the device
/// lists, like Carlita's other clients.
pub const CLIENT_NAME: &str = "Carlita.recorder";
/// Channels a stem can have (a track's dry input takes up to this many).
const MAX_CHANNELS: usize = 8;
/// How much audio the ring buffers hold while the writer catches up.
const BUFFER_SECONDS: usize = 4;
/// How often the files' headers are brought up to date, so a crash or power
/// loss leaves files that play up to there.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

/// What to record of one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSpec {
    pub id: TrackId,
    pub name: String,
    /// Channels of the dry input to record too (0 = none).
    pub dry_channels: usize,
}

/// One file's inputs: a track's output (stereo) or its dry input.
struct Stream {
    ports: Vec<Port<AudioIn>>,
}

enum RtMsg {
    /// Start writing into these, one per stream.
    Start(Vec<Producer<f32>>),
    Stop,
}

struct Shared {
    paused: AtomicBool,
    /// Frames written in this take.
    frames: AtomicU64,
    /// Cycles left out because the writer fell behind.
    dropped: AtomicU64,
}

struct Process {
    streams: Vec<Stream>,
    rx: Consumer<RtMsg>,
    /// Producers handed back when a take ends, to be dropped off the audio
    /// thread.
    back: Producer<Vec<Producer<f32>>>,
    active: Option<Vec<Producer<f32>>>,
    shared: Arc<Shared>,
}

impl jack::ProcessHandler for Process {
    fn process(&mut self, _: &Client, ps: &ProcessScope) -> Control {
        while let Ok(msg) = self.rx.pop() {
            let old = match msg {
                RtMsg::Start(p) => self.active.replace(p),
                RtMsg::Stop => self.active.take(),
            };
            if let Some(old) = old {
                // The queue holds a few; if it is full, they are dropped here
                // (rare, and only frees memory).
                let _ = self.back.push(old);
            }
        }
        let Some(prods) = self.active.as_mut() else {
            return Control::Continue;
        };
        if self.shared.paused.load(Ordering::Relaxed) {
            return Control::Continue;
        }
        let n = ps.n_frames() as usize;
        // Every stream or none, so the stems stay in step.
        let room = self
            .streams
            .iter()
            .zip(prods.iter())
            .all(|(s, p)| p.slots() >= n * s.ports.len());
        if !room {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return Control::Continue;
        }
        for (s, p) in self.streams.iter().zip(prods.iter_mut()) {
            let ch = s.ports.len();
            let mut bufs: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (b, port) in bufs.iter_mut().zip(&s.ports) {
                *b = port.as_slice(ps);
            }
            if let Ok(chunk) = p.write_chunk_uninit(n * ch) {
                chunk.fill_from_iter(
                    (0..n * ch).map(|k| bufs[k % ch].get(k / ch).copied().unwrap_or(0.0)),
                );
            }
        }
        self.shared.frames.fetch_add(n as u64, Ordering::Relaxed);
        Control::Continue
    }
}

/// How a take ended, from the writer thread.
type WriterResult = std::result::Result<(), String>;

struct Take {
    dir: PathBuf,
    /// One per file, so FLAC encoding spreads over the cores.
    writers: Vec<std::thread::JoinHandle<WriterResult>>,
}

/// The recorder's JACK client, open while a project is.
pub struct Recorder {
    client: Option<jack::AsyncClient<(), Process>>,
    name: String,
    tracks: Vec<TrackSpec>,
    sample_rate: u32,
    shared: Arc<Shared>,
    to_rt: Producer<RtMsg>,
    from_rt: Consumer<Vec<Producer<f32>>>,
    take: Option<Take>,
}

impl Recorder {
    /// Opens the JACK client with inputs for `tracks`.
    pub fn open(tracks: Vec<TrackSpec>) -> Result<Self> {
        let (client, _) = Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER)
            .context("could not connect to JACK")?;
        let mut streams = Vec::new();
        for (track, dry, channels) in stream_layout(&tracks) {
            let id = tracks[track].id;
            let ports = (0..channels)
                .map(|c| client.register_port(&recorder_port(id, dry, c), AudioIn::default()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("could not add the recorder's inputs")?;
            streams.push(Stream { ports });
        }
        let shared = Arc::new(Shared {
            paused: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let (to_rt, rx) = RingBuffer::new(8);
        let (back, from_rt) = RingBuffer::new(8);
        let name = client.name().to_string();
        let sample_rate = client.sample_rate();
        let process = Process {
            streams,
            rx,
            back,
            active: None,
            shared: shared.clone(),
        };
        let client = client
            .activate_async((), process)
            .context("could not start the recorder")?;
        Ok(Self {
            client: Some(client),
            name,
            tracks,
            sample_rate,
            shared,
            to_rt,
            from_rt,
            take: None,
        })
    }

    /// The JACK client name (JACK may have changed it if it was taken).
    pub fn client_name(&self) -> &str {
        &self.name
    }

    /// Whether the client has the inputs `tracks` need (names aside).
    pub fn fits(&self, tracks: &[TrackSpec]) -> bool {
        self.tracks.len() == tracks.len()
            && self.tracks.iter().zip(tracks).all(|(a, b)| {
                a.id == b.id && a.dry_channels.min(MAX_CHANNELS) == b.dry_channels.min(MAX_CHANNELS)
            })
    }

    /// Takes the tracks' current names (for the next take's files).
    pub fn set_tracks(&mut self, tracks: Vec<TrackSpec>) {
        if self.fits(&tracks) {
            self.tracks = tracks;
        }
    }

    pub fn recording(&self) -> bool {
        self.take.is_some()
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Length of the take so far.
    pub fn elapsed(&self) -> Duration {
        let frames = self.shared.frames.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.sample_rate.max(1) as f64)
    }

    /// Cycles left out of this take because the disk could not keep up.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// The folder of the take being recorded.
    pub fn take_dir(&self) -> Option<&Path> {
        self.take.as_ref().map(|t| t.dir.as_path())
    }

    /// Starts a new take in a new "Take NN" folder of `project`.
    pub fn start(&mut self, project: &Path, format: RecordFormat) -> Result<PathBuf> {
        if self.take.is_some() {
            bail!("already recording");
        }
        self.collect();
        if self.client.is_none() {
            bail!("the recorder lost its connection to JACK");
        }
        let dir = next_take_dir(project)?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        let names = file_names(&self.tracks);
        let mut prods = Vec::new();
        let mut sinks = Vec::new();
        for (track, dry, channels) in stream_layout(&self.tracks) {
            let ext = stem_file::extension(format);
            let file = dir.join(if dry {
                format!("{} (dry).{ext}", names[track])
            } else {
                format!("{}.{ext}", names[track])
            });
            let stem = StemFile::create(&file, format, channels, self.sample_rate)
                .with_context(|| format!("could not create {}", file.display()))?;
            let (p, c) = RingBuffer::new(self.sample_rate as usize * channels * BUFFER_SECONDS);
            prods.push(p);
            sinks.push((c, stem, file));
        }
        write_take_info(&dir, self.sample_rate, format, &self.tracks);
        let mut writers = Vec::new();
        for sink in sinks {
            let w = std::thread::Builder::new()
                .name("carlita-recorder".into())
                .spawn(move || write_files(vec![sink]));
            match w {
                Ok(w) => writers.push(w),
                Err(e) => {
                    // The ones started end once `prods` is dropped.
                    drop(prods);
                    for w in writers {
                        let _ = w.join();
                    }
                    bail!("could not start the writer: {e}");
                }
            }
        }
        self.shared.frames.store(0, Ordering::Relaxed);
        self.shared.dropped.store(0, Ordering::Relaxed);
        self.shared.paused.store(false, Ordering::Relaxed);
        if self.to_rt.push(RtMsg::Start(prods)).is_err() {
            bail!("the recorder is not responding");
        }
        self.take = Some(Take {
            dir: dir.clone(),
            writers,
        });
        Ok(dir)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Ends the take and waits for its files to be written. Returns the
    /// take's folder.
    pub fn stop(&mut self) -> Result<PathBuf> {
        let Some(mut take) = self.take.take() else {
            bail!("not recording");
        };
        let _ = self.to_rt.push(RtMsg::Stop);
        // The audio thread hands the buffers back within a cycle or two;
        // once they are dropped the writer finishes. Without a running
        // JACK, nothing comes back: deactivating the client frees them.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.from_rt.pop().is_ok() {
                break;
            }
            if Instant::now() > deadline {
                if let Some(c) = self.client.take() {
                    let _ = c.deactivate();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Every writer finishes its file; the first error is reported.
        let mut result: WriterResult = Ok(());
        for w in take.writers.drain(..) {
            let r = w
                .join()
                .unwrap_or_else(|_| Err("the writer stopped".into()));
            if result.is_ok() {
                result = r;
            }
        }
        self.shared.paused.store(false, Ordering::Relaxed);
        match result {
            Ok(()) => Ok(take.dir),
            Err(e) => bail!("{e}"),
        }
    }

    /// Drops buffers the audio thread handed back.
    fn collect(&mut self) {
        while self.from_rt.pop().is_ok() {}
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.take.is_some() {
            let _ = self.stop();
        }
        if let Some(c) = self.client.take() {
            let _ = c.deactivate();
        }
    }
}

/// (track index, dry, channels) of each stream, in the order `Process`
/// has them.
fn stream_layout(tracks: &[TrackSpec]) -> Vec<(usize, bool, usize)> {
    let mut v = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        v.push((i, false, 2));
        let dry = t.dry_channels.min(MAX_CHANNELS);
        if dry > 0 {
            v.push((i, true, dry));
        }
    }
    v
}

/// Writes what the audio thread sends until every buffer is dropped and
/// empty.
fn write_files(mut sinks: Vec<(Consumer<f32>, StemFile, PathBuf)>) -> WriterResult {
    let mut error: Option<String> = None;
    let mut last_flush = Instant::now();
    loop {
        let mut done = true;
        for (c, stem, file) in sinks.iter_mut() {
            let abandoned = c.is_abandoned();
            let n = c.slots();
            if n > 0 {
                if let Ok(chunk) = c.read_chunk(n) {
                    let (a, b) = chunk.as_slices();
                    if error.is_none() {
                        if let Err(e) = stem.write(a).and_then(|()| stem.write(b)) {
                            error = Some(format!("{}: {e}", file.display()));
                        }
                    }
                    chunk.commit_all();
                }
            }
            if !(abandoned && c.is_empty()) {
                done = false;
            }
        }
        if done {
            break;
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            last_flush = Instant::now();
            for (_, stem, file) in sinks.iter_mut() {
                if let Err(e) = stem.flush() {
                    error.get_or_insert_with(|| format!("{}: {e}", file.display()));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for (_, stem, file) in sinks {
        if let Err(e) = stem.finish() {
            error.get_or_insert_with(|| format!("{}: {e}", file.display()));
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// "Take 01", "Take 02", ...: the first number after every take there is.
fn next_take_dir(project: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(project)
        .with_context(|| format!("could not create {}", project.display()))?;
    let last = std::fs::read_dir(project)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("Take ")?
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    Ok(project.join(format!("Take {:02}", last + 1)))
}

/// The tracks' names as file names: "01 Guitar", "02 Bass", ... (numbered
/// so they sort in track order and two tracks of one name don't clash).
fn file_names(tracks: &[TrackSpec]) -> Vec<String> {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{:02} {}", i + 1, safe_name(&t.name)))
        .collect()
}

/// A name for a file or folder: no path separators or characters other
/// systems refuse, not empty, not starting with a dot.
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let s = s.trim_start_matches('.').trim().to_string();
    if s.is_empty() {
        "Untitled".into()
    } else {
        s
    }
}

/// A note next to the files, for whoever opens the take in a DAW.
fn write_take_info(dir: &Path, sample_rate: u32, format: RecordFormat, tracks: &[TrackSpec]) {
    let names = file_names(tracks);
    let mut text = format!(
        "Recorded by Carlita {}\nSample rate: {sample_rate} Hz, {}{}\n\
         Every file starts at the same moment. Track files are what the track \
         played (after its plugins); \"(dry)\" files are its inputs before any \
         plugin, so they are ahead of the track file by the chain's latency.\n\nTracks:\n",
        env!("CARGO_PKG_VERSION"),
        stem_file::describe(format),
        match format {
            RecordFormat::Wav => " (files over 4 GB are RF64, the 64-bit WAV)",
            RecordFormat::Flac => "",
        },
    );
    for (n, t) in names.iter().zip(tracks) {
        text += &format!(
            "  {n}{}\n",
            if t.dry_channels > 0 { " (+ dry)" } else { "" }
        );
    }
    if let Err(e) = std::fs::write(dir.join("take.txt"), text) {
        log::warn!("could not write take.txt: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("Lead / Rhythm"), "Lead - Rhythm");
        assert_eq!(safe_name("  ..hidden "), "hidden");
        assert_eq!(safe_name(""), "Untitled");
        assert_eq!(safe_name("a:b?c"), "a-b-c");
    }

    #[test]
    fn takes_are_numbered_after_the_last() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            next_take_dir(dir.path()).unwrap(),
            dir.path().join("Take 01")
        );
        std::fs::create_dir(dir.path().join("Take 01")).unwrap();
        std::fs::create_dir(dir.path().join("Take 07")).unwrap();
        std::fs::create_dir(dir.path().join("Other")).unwrap();
        assert_eq!(
            next_take_dir(dir.path()).unwrap(),
            dir.path().join("Take 08")
        );
    }

    #[test]
    fn writer_writes_interleaved_frames() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.wav");
        let wav = StemFile::create(&file, RecordFormat::Wav, 2, 48_000).unwrap();
        let (mut p, c) = RingBuffer::new(64);
        for s in [0.1f32, -0.1, 0.2, -0.2] {
            p.push(s).unwrap();
        }
        drop(p);
        write_files(vec![(c, wav, file.clone())]).unwrap();
        let r = hound::WavReader::open(&file).unwrap();
        assert_eq!(r.duration(), 2);
        let s: Vec<f32> = r.into_samples().map(|s| s.unwrap()).collect();
        assert_eq!(s, vec![0.1, -0.1, 0.2, -0.2]);
    }

    #[test]
    fn file_names_are_numbered() {
        let t = |n: &str| TrackSpec {
            id: TrackId(1),
            name: n.into(),
            dry_channels: 0,
        };
        assert_eq!(
            file_names(&[t("Gtr"), t("Gtr")]),
            vec!["01 Gtr".to_string(), "02 Gtr".to_string()]
        );
    }
}
