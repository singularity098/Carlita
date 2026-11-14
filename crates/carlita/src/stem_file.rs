//! The files stems are written to: 32-bit float WAV, which turns into RF64
//! once it passes 4 GB, or 24-bit FLAC.
//!
//! Both keep their headers playable while they grow: `flush` brings the
//! header up to date, so a crash or power loss leaves a file that plays up
//! to the last flush.

use carlita_core::settings::RecordFormat;
use flacenc::bitsink::MemSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// File name extension of a format.
pub fn extension(format: RecordFormat) -> &'static str {
    match format {
        RecordFormat::Wav => "wav",
        RecordFormat::Flac => "flac",
    }
}

/// How a format is described to people ("32-bit float WAV").
pub fn describe(format: RecordFormat) -> &'static str {
    match format {
        RecordFormat::Wav => "32-bit float WAV",
        RecordFormat::Flac => "24-bit FLAC",
    }
}

/// One stem being written: interleaved frames in, a file out.
pub enum StemFile {
    Wav(WavFile),
    Flac(Box<FlacFile>),
}

impl StemFile {
    pub fn create(
        path: &Path,
        format: RecordFormat,
        channels: usize,
        sample_rate: u32,
    ) -> io::Result<Self> {
        Ok(match format {
            RecordFormat::Wav => Self::Wav(WavFile::create(path, channels, sample_rate)?),
            RecordFormat::Flac => {
                Self::Flac(Box::new(FlacFile::create(path, channels, sample_rate)?))
            }
        })
    }

    /// Appends interleaved samples (whole frames, or the rest of one
    /// started by the last call).
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        match self {
            Self::Wav(f) => f.write(samples),
            Self::Flac(f) => f.write(samples),
        }
    }

    /// Writes what is buffered and brings the header up to date.
    pub fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Wav(f) => f.flush(),
            Self::Flac(f) => f.flush(),
        }
    }

    /// Writes the rest and closes the file.
    pub fn finish(self) -> io::Result<()> {
        match self {
            Self::Wav(f) => f.finish(),
            Self::Flac(f) => f.finish(),
        }
    }
}

// ================================================================== WAV

/// Where the JUNK chunk that becomes ds64 is.
const DS64_AT: u64 = 12;
/// A `JUNK` chunk just big enough to become `ds64` (EBU Tech 3306): RIFF
/// size, data size and sample count as 64-bit, and an empty table.
const DS64_SIZE: u32 = 28;
/// The plain WAV size fields' limit; past it the file is RF64.
const WAV_LIMIT: u64 = u32::MAX as u64;

/// A 32-bit float WAV file. Up to 4 GB it is an ordinary WAV; past that it
/// becomes RF64 (the 64-bit WAV of EBU Tech 3306, read by Audacity, Ardour,
/// Reaper, ffmpeg, SoX and any program using libsndfile). Only its first
/// letters and the sizes change: the audio stays where it was.
pub struct WavFile {
    file: BufWriter<File>,
    channels: usize,
    /// Where the `fact` chunk's frame count and the data size are.
    fact_at: u64,
    data_size_at: u64,
    /// Bytes of audio written.
    data: u64,
    /// The size at which the file turns into RF64 (lower in tests).
    limit: u64,
}

impl WavFile {
    pub fn create(path: &Path, channels: usize, sample_rate: u32) -> io::Result<Self> {
        let mut file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let ch = channels as u16;
        let block_align = ch * 4;
        let mut h = Vec::with_capacity(128);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"JUNK");
        h.extend_from_slice(&DS64_SIZE.to_le_bytes());
        h.extend_from_slice(&[0; DS64_SIZE as usize]);
        // Plain IEEE float up to stereo; more channels need the extensible
        // form.
        let extensible = channels > 2;
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&(if extensible { 40u32 } else { 18 }).to_le_bytes());
        h.extend_from_slice(&(if extensible { 0xFFFEu16 } else { 3 }).to_le_bytes());
        h.extend_from_slice(&ch.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        if extensible {
            h.extend_from_slice(&22u16.to_le_bytes());
            h.extend_from_slice(&32u16.to_le_bytes()); // valid bits
            h.extend_from_slice(&0u32.to_le_bytes()); // no speaker layout
                                                      // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            h.extend_from_slice(&[
                0x03, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
            ]);
        } else {
            h.extend_from_slice(&0u16.to_le_bytes());
        }
        // Non-PCM WAVs carry a frame count.
        h.extend_from_slice(b"fact");
        h.extend_from_slice(&4u32.to_le_bytes());
        let fact_at = h.len() as u64;
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"data");
        let data_size_at = h.len() as u64;
        h.extend_from_slice(&0u32.to_le_bytes());
        file.write_all(&h)?;
        let mut wav = Self {
            file,
            channels,
            fact_at,
            data_size_at,
            data: 0,
            limit: WAV_LIMIT,
        };
        wav.update_header()?;
        Ok(wav)
    }

    fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        for s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.data += samples.len() as u64 * 4;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.update_header()
    }

    fn finish(mut self) -> io::Result<()> {
        self.update_header()?;
        self.file
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()
    }

    /// Writes the sizes (as RF64 once they no longer fit) and puts the
    /// write position back at the end.
    fn update_header(&mut self) -> io::Result<()> {
        let riff = self.data_size_at + 4 + self.data - 8;
        let frames = self.data / (self.channels as u64 * 4);
        let f = &mut self.file;
        if riff <= self.limit {
            f.seek(SeekFrom::Start(0))?;
            f.write_all(b"RIFF")?;
            f.write_all(&(riff as u32).to_le_bytes())?;
            f.seek(SeekFrom::Start(self.fact_at))?;
            f.write_all(&(frames as u32).to_le_bytes())?;
            f.seek(SeekFrom::Start(self.data_size_at))?;
            f.write_all(&(self.data as u32).to_le_bytes())?;
        } else {
            f.seek(SeekFrom::Start(DS64_AT))?;
            f.write_all(b"ds64")?;
            f.write_all(&DS64_SIZE.to_le_bytes())?;
            f.write_all(&riff.to_le_bytes())?;
            f.write_all(&self.data.to_le_bytes())?;
            f.write_all(&frames.to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?;
            f.seek(SeekFrom::Start(self.fact_at))?;
            f.write_all(&u32::MAX.to_le_bytes())?;
            f.seek(SeekFrom::Start(self.data_size_at))?;
            f.write_all(&u32::MAX.to_le_bytes())?;
            // Last, so a file that says RF64 always has its ds64.
            f.flush()?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(b"RF64")?;
            f.write_all(&u32::MAX.to_le_bytes())?;
        }
        f.seek(SeekFrom::End(0))?;
        f.flush()
    }
}

// ================================================================= FLAC

/// Samples per channel in a FLAC frame (the reference encoder's default).
const FLAC_BLOCK: usize = 4096;
const FLAC_BITS: usize = 24;
/// Where STREAMINFO's fields start: after "fLaC" and the block header.
const STREAMINFO_AT: u64 = 8;

/// A 24-bit FLAC file. FLAC has no practical length limit (2^36 frames,
/// over 190 hours at 96 kHz). Samples above 0 dBFS are clipped.
pub struct FlacFile {
    file: BufWriter<File>,
    channels: usize,
    sample_rate: u32,
    config: Verified<config::Encoder>,
    info: StreamInfo,
    /// Interleaved samples not yet making a whole frame.
    pending: Vec<i32>,
    framebuf: FrameBuf,
    /// Counts frames and keeps the MD5 of the audio.
    context: Context,
    sink: MemSink<u8>,
    frames: u64,
    min_frame: u32,
    max_frame: u32,
}

impl FlacFile {
    pub fn create(path: &Path, channels: usize, sample_rate: u32) -> io::Result<Self> {
        let err = |e: flacenc::error::VerifyError| io::Error::other(e.to_string());
        let mut cfg = config::Encoder::default();
        cfg.block_size = FLAC_BLOCK;
        let config = cfg.into_verified().map_err(|(_, e)| err(e))?;
        let info = StreamInfo::new(sample_rate as usize, channels, FLAC_BITS).map_err(err)?;
        let framebuf = FrameBuf::with_size(channels, FLAC_BLOCK).map_err(err)?;
        let mut flac = Self {
            file: BufWriter::with_capacity(1 << 20, File::create(path)?),
            channels,
            sample_rate,
            config,
            info,
            pending: Vec::with_capacity(FLAC_BLOCK * channels),
            framebuf,
            context: Context::new(FLAC_BITS, channels),
            sink: MemSink::with_capacity(FLAC_BLOCK * channels * FLAC_BITS),
            frames: 0,
            min_frame: 0,
            max_frame: 0,
        };
        flac.file.write_all(b"fLaC")?;
        // The only metadata block (last-block flag set), 34 bytes long.
        flac.file.write_all(&[0x80, 0, 0, 34])?;
        flac.file.write_all(&flac.stream_info(false))?;
        flac.file.flush()?;
        Ok(flac)
    }

    fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let full = FLAC_BLOCK * self.channels;
        let scale = ((1i32 << (FLAC_BITS - 1)) - 1) as f32;
        for s in samples {
            // NaN becomes 0.
            self.pending
                .push((s.clamp(-1.0, 1.0) * scale).round() as i32);
            if self.pending.len() == full {
                self.encode()?;
            }
        }
        Ok(())
    }

    /// Encodes `pending` as one frame.
    fn encode(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let err = |e: &dyn std::fmt::Display| io::Error::other(e.to_string());
        // A frame needs whole frames of samples.
        self.pending
            .truncate(self.pending.len() / self.channels * self.channels);
        self.framebuf
            .fill_interleaved(&self.pending)
            .map_err(|e| err(&e))?;
        self.context
            .fill_interleaved(&self.pending)
            .map_err(|e| err(&e))?;
        let number = self.context.current_frame_number().unwrap_or(0);
        let frame =
            flacenc::encode_fixed_size_frame(&self.config, &self.framebuf, number, &self.info)
                .map_err(|e| err(&e))?;
        self.sink.clear();
        frame.write(&mut self.sink).map_err(|e| err(&e))?;
        let bytes = self.sink.as_slice();
        self.file.write_all(bytes)?;
        let size = bytes.len() as u32;
        self.min_frame = if self.frames == 0 {
            size
        } else {
            self.min_frame.min(size)
        };
        self.max_frame = self.max_frame.max(size);
        self.frames += (self.pending.len() / self.channels) as u64;
        self.pending.clear();
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.update_header(false)
    }

    fn finish(mut self) -> io::Result<()> {
        self.encode()?;
        self.update_header(true)?;
        self.file
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()
    }

    fn update_header(&mut self, with_md5: bool) -> io::Result<()> {
        let info = self.stream_info(with_md5);
        let f = &mut self.file;
        f.seek(SeekFrom::Start(STREAMINFO_AT))?;
        f.write_all(&info)?;
        f.seek(SeekFrom::End(0))?;
        f.flush()
    }

    /// The STREAMINFO block's 34 bytes. The MD5 is left at zero ("not
    /// known") until the file is finished.
    fn stream_info(&self, with_md5: bool) -> [u8; 34] {
        let mut b = [0u8; 34];
        let block = (FLAC_BLOCK as u16).to_be_bytes();
        b[0..2].copy_from_slice(&block);
        b[2..4].copy_from_slice(&block);
        b[4..7].copy_from_slice(&self.min_frame.to_be_bytes()[1..]);
        b[7..10].copy_from_slice(&self.max_frame.to_be_bytes()[1..]);
        // Sample rate (20 bits), channels - 1 (3), bits - 1 (5), total
        // samples (36).
        let packed: u64 = (u64::from(self.sample_rate) << 44)
            | (((self.channels as u64) - 1) << 41)
            | (((FLAC_BITS as u64) - 1) << 36)
            | (self.frames & ((1 << 36) - 1));
        b[10..18].copy_from_slice(&packed.to_be_bytes());
        if with_md5 {
            b[18..34].copy_from_slice(&self.context.md5_digest());
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| ((i as f32) * 0.001).sin() * 0.5)
            .collect()
    }

    #[test]
    fn wav_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        for channels in [1, 2, 6] {
            let path = dir.path().join(format!("{channels}.wav"));
            let s = samples(1000, channels);
            let mut f = StemFile::create(&path, RecordFormat::Wav, channels, 48_000).unwrap();
            f.write(&s[..300]).unwrap();
            f.flush().unwrap();
            f.write(&s[300..]).unwrap();
            f.finish().unwrap();
            let r = hound::WavReader::open(&path).unwrap();
            assert_eq!(r.spec().channels as usize, channels);
            assert_eq!(r.spec().sample_format, hound::SampleFormat::Float);
            assert_eq!(r.duration(), 1000);
            let back: Vec<f32> = r.into_samples().map(|s| s.unwrap()).collect();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn wav_plays_up_to_the_last_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let mut f = WavFile::create(&path, 2, 48_000).unwrap();
        f.write(&samples(100, 2)).unwrap();
        f.flush().unwrap();
        f.write(&samples(50, 2)).unwrap();
        // As if the power went off here.
        f.file.flush().unwrap();
        let r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.duration(), 100);
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn wav_becomes_rf64_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let mut f = WavFile::create(&path, 2, 48_000).unwrap();
        f.limit = 1000;
        let s = samples(100, 2);
        f.write(&s).unwrap();
        f.flush().unwrap();
        // Still small enough: a plain WAV with a JUNK chunk.
        assert_eq!(&std::fs::read(&path).unwrap()[..4], b"RIFF");
        f.write(&s).unwrap();
        let (data_at, fact_at) = (f.data_size_at as usize + 4, f.fact_at as usize);
        f.finish().unwrap();
        let b = std::fs::read(&path).unwrap();
        assert_eq!(&b[0..4], b"RF64");
        assert_eq!(u32_at(&b, 4), u32::MAX);
        assert_eq!(&b[12..16], b"ds64");
        assert_eq!(u64_at(&b, 20), b.len() as u64 - 8);
        assert_eq!(u64_at(&b, 28), 1600);
        assert_eq!(u64_at(&b, 36), 200);
        assert_eq!(u32_at(&b, fact_at), u32::MAX);
        assert_eq!(u32_at(&b, data_at - 4), u32::MAX);
        let back: Vec<f32> = b[data_at..]
            .chunks(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(back.len(), 400);
        assert_eq!(&back[..200], &s[..]);
    }

    #[test]
    fn flac_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        for channels in [1, 2, 8] {
            let path = dir.path().join(format!("{channels}.flac"));
            // Not a whole number of FLAC frames, written in odd pieces.
            let frames = FLAC_BLOCK * 2 + 123;
            let mut s = samples(frames, channels);
            s[0] = 1.5; // clipped
            let n = s.len();
            s[1 % n] = -1.0;
            let mut f = StemFile::create(&path, RecordFormat::Flac, channels, 96_000).unwrap();
            for piece in s.chunks(1000 * channels + 7) {
                f.write(piece).unwrap();
            }
            f.flush().unwrap();
            f.finish().unwrap();
            let mut r = claxon::FlacReader::open(&path).unwrap();
            let info = r.streaminfo();
            assert_eq!(info.channels as usize, channels);
            assert_eq!(info.sample_rate, 96_000);
            assert_eq!(info.bits_per_sample, 24);
            assert_eq!(info.samples, Some(frames as u64));
            assert!(info.md5sum.iter().any(|&b| b != 0));
            let back: Vec<i32> = r.samples().map(|s| s.unwrap()).collect();
            assert_eq!(back.len(), s.len());
            let scale = 8_388_607.0;
            for (a, b) in s.iter().zip(&back) {
                assert_eq!((a.clamp(-1.0, 1.0) * scale).round() as i32, *b);
            }
        }
    }

    #[test]
    fn flac_plays_up_to_the_last_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let mut f = FlacFile::create(&path, 2, 48_000).unwrap();
        f.write(&samples(FLAC_BLOCK * 3, 2)).unwrap();
        f.flush().unwrap();
        f.write(&samples(FLAC_BLOCK, 2)).unwrap();
        // As if the power went off here.
        f.file.flush().unwrap();
        let mut r = claxon::FlacReader::open(&path).unwrap();
        assert_eq!(r.streaminfo().samples, Some(FLAC_BLOCK as u64 * 3));
        assert_eq!(r.samples().count(), FLAC_BLOCK * 4 * 2);
    }

    /// Prints how much faster than real time the FLAC encoder runs
    /// (`cargo test --release -- --ignored --nocapture flac_speed`).
    #[test]
    #[ignore]
    fn flac_speed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let seconds = 60;
        let s: Vec<f32> = (0..96_000 * seconds * 2)
            .map(|i| ((i as f32) * 0.0007).sin() * 0.3 + ((i * 7919 % 1000) as f32 / 1e4))
            .collect();
        let start = std::time::Instant::now();
        let mut f = StemFile::create(&path, RecordFormat::Flac, 2, 96_000).unwrap();
        f.write(&s).unwrap();
        f.finish().unwrap();
        let t = start.elapsed().as_secs_f64();
        let size = std::fs::metadata(&path).unwrap().len() as f64;
        println!(
            "{seconds} s of 96 kHz stereo in {t:.2} s ({:.0}x real time), {:.0}% of 32-bit WAV",
            seconds as f64 / t,
            100.0 * size / (s.len() * 4) as f64
        );
    }
}
