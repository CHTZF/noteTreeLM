//! AudioStore — single owner of all PCM data for one meeting session.
//!
//! # Invariants
//! - Audio is written to the backing file **before** being evicted from the memory ring buffer.
//!   This guarantees that `slice()` always succeeds for any range that was ever committed.
//! - `slice()` returns samples from memory if available, falls back to the file transparently.
//! - Append-only: samples are never modified after being committed.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SAMPLE_RATE: u64 = 16_000;
/// Keep last N seconds of audio in memory ring buffer; older audio lives on file only.
const MEMORY_WINDOW_MS: u64 = 5 * 60 * 1000; // 5 minutes
const MEMORY_WINDOW_SAMPLES: usize = (SAMPLE_RATE * MEMORY_WINDOW_MS / 1000) as usize;

/// WAV header size in bytes (standard 44-byte PCM header).
const WAV_HEADER_BYTES: u64 = 44;

struct Inner {
    /// Total samples written (monotonically increasing, never reset).
    total_samples: u64,
    /// Ring buffer holding the most recent MEMORY_WINDOW_SAMPLES samples.
    /// VecDeque allows O(1) pop_front (eviction) and O(1) push_back (insertion).
    ring: VecDeque<i16>,
    /// Sample index of ring[0] in the global timeline.
    ring_offset: u64,
    /// Open file handle; WAV header is pre-written, we seek+overwrite data size on close.
    file: Option<File>,
    /// File path (for read-back in slice()).
    file_path: PathBuf,
}

/// Cloneable handle to the AudioStore.
#[derive(Clone)]
pub struct AudioStore(Arc<Mutex<Inner>>);

impl AudioStore {
    /// Create a new AudioStore backed by `wav_path`.
    /// Writes an initial WAV header; data is appended on each `commit()`.
    pub fn create(wav_path: &Path) -> std::io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(wav_path)?;

        // Write placeholder WAV header (data size = 0, will be patched on drop).
        write_wav_header(&mut file, 0)?;

        let inner = Inner {
            total_samples: 0,
            ring: VecDeque::with_capacity(MEMORY_WINDOW_SAMPLES),
            ring_offset: 0,
            file: Some(file),
            file_path: wav_path.to_path_buf(),
        };
        Ok(AudioStore(Arc::new(Mutex::new(inner))))
    }

    /// Commit new samples. Thread-safe; called from the VAD/audio-capture thread.
    ///
    /// **This must be called before writing the corresponding segment to the DB.**
    /// That ordering guarantee means SpeakerEngine can always read from `slice()` for
    /// any segment it sees in the DB.
    pub fn commit(&self, samples: &[i16]) {
        let mut g = self.0.lock().unwrap();

        // 1. Write to file first (invariant: file write before memory eviction).
        if let Some(ref mut f) = g.file {
            let bytes: Vec<u8> = samples.iter()
                .flat_map(|&s| s.to_le_bytes())
                .collect();
            let _ = f.write_all(&bytes);
        }

        // 2. Append to ring buffer with O(1) bulk eviction.
        let cap = MEMORY_WINDOW_SAMPLES;
        let n = samples.len();

        if n >= cap {
            // Incoming batch exceeds the entire window: keep only the last `cap` samples.
            let skip = n - cap;
            let evicted = g.ring.len() + skip;
            g.ring.drain(..);
            g.ring_offset += evicted as u64;
            g.ring.extend(samples[skip..].iter().copied());
        } else {
            let to_evict = (g.ring.len() + n).saturating_sub(cap);
            if to_evict > 0 {
                g.ring.drain(..to_evict);
                g.ring_offset += to_evict as u64;
            }
            g.ring.extend(samples.iter().copied());
        }

        g.total_samples += samples.len() as u64;
    }

    /// Total samples received since store creation (monotonically increasing).
    pub fn total_samples(&self) -> u64 {
        self.0.lock().unwrap().total_samples
    }

    /// Compute absolute meeting time (ms) for a sample at `total_sample_index`.
    pub fn sample_to_ms(total_sample_index: u64) -> u64 {
        total_sample_index * 1000 / SAMPLE_RATE
    }

    /// Slice audio between `from_ms` and `to_ms` (milliseconds from meeting start).
    /// Returns the PCM samples. If range extends beyond committed audio, returns
    /// whatever is available.
    ///
    /// Transparently reads from memory ring buffer if available, otherwise reads from file.
    pub fn slice(&self, from_ms: u64, to_ms: u64) -> Vec<i16> {
        let g = self.0.lock().unwrap();

        let from_sample = (from_ms * SAMPLE_RATE / 1000) as u64;
        let to_sample   = ((to_ms  * SAMPLE_RATE / 1000) as u64).min(g.total_samples);

        if from_sample >= to_sample {
            return vec![];
        }

        let n = (to_sample - from_sample) as usize;

        // Fast path: entirely in memory ring buffer.
        // VecDeque doesn't support range slicing directly; use iter().skip().take().
        if from_sample >= g.ring_offset {
            let ring_start = (from_sample - g.ring_offset) as usize;
            let ring_end   = ring_start + n;
            if ring_end <= g.ring.len() {
                return g.ring.iter().skip(ring_start).take(n).copied().collect();
            }
        }

        // Slow path: read from file.
        drop(g); // release lock before file I/O
        self.slice_from_file(from_sample, to_sample)
    }

    fn slice_from_file(&self, from_sample: u64, to_sample: u64) -> Vec<i16> {
        let g = self.0.lock().unwrap();
        let path = g.file_path.clone();
        drop(g);

        let n = (to_sample - from_sample) as usize;
        let byte_offset = WAV_HEADER_BYTES + from_sample * 2; // 2 bytes per i16 sample

        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return vec![],
        };
        if file.seek(SeekFrom::Start(byte_offset)).is_err() {
            return vec![];
        }

        let mut buf = vec![0u8; n * 2];
        use std::io::Read;
        let bytes_read = match file.read(&mut buf) {
            Ok(b) => b,
            Err(_) => return vec![],
        };

        buf[..bytes_read]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// Finalise the WAV file by patching the data-size field in the header.
    /// Call once when the meeting ends.
    pub fn finalize(&self) {
        let mut g = self.0.lock().unwrap();
        let data_bytes = (g.total_samples * 2) as u32;
        if let Some(ref mut f) = g.file {
            // Patch RIFF chunk size at offset 4 and data chunk size at offset 40.
            let _ = f.seek(SeekFrom::Start(4));
            let _ = f.write_all(&(36 + data_bytes).to_le_bytes());
            let _ = f.seek(SeekFrom::Start(40));
            let _ = f.write_all(&data_bytes.to_le_bytes());
            let _ = f.flush();
        }
    }
}

fn write_wav_header(f: &mut File, data_bytes: u32) -> std::io::Result<()> {
    let sample_rate: u32 = SAMPLE_RATE as u32;
    let channels: u16 = 1;
    let bits: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
    let block_align: u16 = channels * bits / 8;

    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;        // PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}
