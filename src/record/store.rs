//! The DVR buffer: every encoded packet, on disk, indexed in memory.
//!
//! Packets are appended to 10-second segment files in a private temp
//! directory. Packets older than the rewind window are dropped (back to a
//! keyframe, so the buffer never exceeds the window) and segment files are
//! deleted once nothing in them is still buffered. The
//! pipeline thread writes; the player reads through its own file handles.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Rational, codec};

const SEGMENT_SECS: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
}

/// Codec parameters, owned (not borrowed from a live context).
pub struct OwnedParameters(codec::Parameters);

// SAFETY: built with `Parameters::from(&context)`, which allocates a fresh
// AVCodecParameters with no owner; it's plain data, never shared mutably.
unsafe impl Send for OwnedParameters {}
unsafe impl Sync for OwnedParameters {}

impl OwnedParameters {
    pub fn new(params: codec::Parameters) -> Self {
        Self(params)
    }

    pub fn get(&self) -> codec::Parameters {
        self.0.clone()
    }
}

pub struct StreamDesc {
    pub kind: StreamKind,
    pub time_base: Rational,
    pub params: OwnedParameters,
}

impl StreamDesc {
    pub fn seconds(&self, pts: i64) -> f64 {
        pts as f64 * self.time_base.0 as f64 / self.time_base.1 as f64
    }

    pub fn pts(&self, seconds: f64) -> i64 {
        (seconds * self.time_base.1 as f64 / self.time_base.0 as f64).round() as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub pts: i64,
    pub duration: i64,
    pub key: bool,
    segment: u32,
    offset: u64,
    len: u32,
}

#[derive(Default)]
struct Index {
    /// Entries per stream, in pts order.
    streams: Vec<VecDeque<Entry>>,
    oldest_segment: u32,
}

struct Shared {
    dir: PathBuf,
    streams: Vec<StreamDesc>,
    index: RwLock<Index>,
    window: Duration,
    bytes: AtomicU64,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Read access to the buffer, cheap to clone.
#[derive(Clone)]
pub struct Store(Arc<Shared>);

/// The single writer, owned by the pipeline thread.
pub struct StoreWriter {
    store: Store,
    file: File,
    segment: u32,
    segment_start: f64,
    offset: u64,
}

impl Store {
    pub fn create(streams: Vec<StreamDesc>, window: Duration) -> Result<StoreWriter> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        remove_stale_buffers();
        let dir = std::env::temp_dir().join(format!(
            "frameback-dvr-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let index = Index {
            streams: streams.iter().map(|_| VecDeque::new()).collect(),
            oldest_segment: 0,
        };
        let store = Store(Arc::new(Shared {
            dir,
            streams,
            index: RwLock::new(index),
            window,
            bytes: AtomicU64::new(0),
        }));
        let file = File::create(store.segment_path(0))?;
        Ok(StoreWriter { store, file, segment: 0, segment_start: 0.0, offset: 0 })
    }

    fn segment_path(&self, segment: u32) -> PathBuf {
        self.0.dir.join(format!("{segment:06}.seg"))
    }

    pub fn streams(&self) -> &[StreamDesc] {
        &self.0.streams
    }

    pub fn stream_of(&self, kind: StreamKind) -> Option<usize> {
        self.0.streams.iter().position(|s| s.kind == kind)
    }

    /// Bytes currently on disk.
    pub fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }

    /// Oldest and newest buffered video times, in seconds.
    pub fn video_range(&self) -> Option<(f64, f64)> {
        let stream = self.stream_of(StreamKind::Video)?;
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        let desc = &self.0.streams[stream];
        Some((desc.seconds(entries.front()?.pts), desc.seconds(entries.back()?.pts)))
    }

    /// The last entry at or before `pts`, plus every entry from the keyframe
    /// that precedes it: exactly what a decoder needs to show that frame.
    pub fn decode_run(&self, stream: usize, pts: i64) -> Vec<Entry> {
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        let end = entries.partition_point(|e| e.pts <= pts);
        if end == 0 {
            return entries.front().map(|e| vec![*e]).unwrap_or_default();
        }
        let start = (0..end).rev().find(|&i| entries[i].key).unwrap_or(0);
        entries.range(start..end).copied().collect()
    }

    /// The newest keyframe at or before `pts`.
    pub fn keyframe_at_or_before(&self, stream: usize, pts: i64) -> Option<Entry> {
        self.decode_run(stream, pts).first().copied().filter(|e| e.key && e.pts <= pts)
    }

    /// Video frames from `from` to `to` inclusive (session seconds).
    pub fn frames_between(&self, from: f64, to: f64) -> usize {
        let Some(stream) = self.stream_of(StreamKind::Video) else { return 0 };
        let desc = &self.0.streams[stream];
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        let lo = entries.partition_point(|e| e.pts < desc.pts(from));
        let hi = entries.partition_point(|e| e.pts <= desc.pts(to));
        hi.saturating_sub(lo)
    }

    /// The first entry strictly after `pts`.
    pub fn next_after(&self, stream: usize, pts: i64) -> Option<Entry> {
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        entries.get(entries.partition_point(|e| e.pts <= pts)).copied()
    }

    /// The entry `n` positions away from the one at or before `pts` (clamped).
    pub fn offset_from(&self, stream: usize, pts: i64, n: i64) -> Option<Entry> {
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        if entries.is_empty() {
            return None;
        }
        let here = entries.partition_point(|e| e.pts <= pts).saturating_sub(1) as i64;
        let target = (here + n).clamp(0, entries.len() as i64 - 1);
        entries.get(target as usize).copied()
    }

    /// Entries with `from <= pts < to`.
    pub fn range(&self, stream: usize, from: i64, to: i64) -> Vec<Entry> {
        let index = self.0.index.read().unwrap();
        let entries = &index.streams[stream];
        let start = entries.partition_point(|e| e.pts < from);
        let end = entries.partition_point(|e| e.pts < to);
        entries.range(start..end).copied().collect()
    }

    pub fn reader(&self) -> StoreReader {
        StoreReader { store: self.clone(), files: HashMap::new() }
    }
}

impl StoreWriter {
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn append(&mut self, stream: usize, pts: i64, duration: i64, key: bool, data: &[u8]) -> Result<()> {
        let shared = &self.store.0;
        let time = shared.streams[stream].seconds(pts);
        let is_video = shared.streams[stream].kind == StreamKind::Video;
        // Start new segments on video keyframes so pruning never strands a GOP.
        if is_video && key && time - self.segment_start >= SEGMENT_SECS {
            self.segment += 1;
            self.segment_start = time;
            self.offset = 0;
            self.file = File::create(self.store.segment_path(self.segment))?;
        }
        self.file.write_all(data).context("writing DVR buffer")?;
        let entry = Entry {
            pts,
            duration,
            key,
            segment: self.segment,
            offset: self.offset,
            len: data.len() as u32,
        };
        self.offset += data.len() as u64;
        shared.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        shared.index.write().unwrap().streams[stream].push_back(entry);
        if is_video {
            self.prune(time);
        }
        Ok(())
    }

    /// Keeps the buffer within the rewind window: drops everything before the
    /// first video keyframe inside it (so the oldest frame always decodes),
    /// and deletes segment files nothing refers to any more.
    fn prune(&mut self, now: f64) {
        let shared = &self.store.0;
        let cutoff = now - shared.window.as_secs_f64();
        let mut index = shared.index.write().unwrap();
        let Some(video) = shared.streams.iter().position(|d| d.kind == StreamKind::Video) else {
            return;
        };
        let desc = &shared.streams[video];
        let Some(start) = index.streams[video]
            .iter()
            .find(|e| e.key && desc.seconds(e.pts) >= cutoff)
            .map(|e| desc.seconds(e.pts))
        else {
            return;
        };
        for (entries, desc) in index.streams.iter_mut().zip(&shared.streams) {
            while entries.front().is_some_and(|e| desc.seconds(e.pts) < start - 1e-9) {
                let e = entries.pop_front().unwrap();
                shared.bytes.fetch_sub(e.len as u64, Ordering::Relaxed);
            }
        }
        let in_use = index.streams.iter().filter_map(|e| e.front()).map(|e| e.segment).min();
        let first_kept = in_use.unwrap_or(self.segment);
        while index.oldest_segment < first_kept {
            let _ = fs::remove_file(self.store.segment_path(index.oldest_segment));
            index.oldest_segment += 1;
        }
    }
}

/// Deletes buffers left behind by frameback processes that were killed or crashed.
/// A live buffer is written to continuously, so anything idle for two
/// minutes is abandoned.
fn remove_stale_buffers() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else { return };
    let stale_after = Duration::from_secs(120);
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("frameback-dvr-") {
            continue;
        }
        // The folder's own time counts too: a new buffer's folder exists
        // briefly before its first file does.
        let newest = fs::read_dir(entry.path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|f| f.metadata().ok()?.modified().ok())
            .chain(entry.metadata().ok().and_then(|m| m.modified().ok()))
            .max();
        let idle = newest.and_then(|t| t.elapsed().ok()).unwrap_or(stale_after);
        if idle >= stale_after {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub struct StoreReader {
    store: Store,
    files: HashMap<u32, File>,
}

impl StoreReader {
    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        if !self.files.contains_key(&entry.segment) {
            // Segments only ever get deleted from the front; forget older handles.
            self.files.retain(|&s, _| s > entry.segment.saturating_sub(2));
            let file = File::open(self.store.segment_path(entry.segment))
                .context("DVR segment no longer buffered")?;
            self.files.insert(entry.segment, file);
        }
        let file = self.files.get_mut(&entry.segment).unwrap();
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut data = vec![0; entry.len as usize];
        file.read_exact(&mut data)?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_store(window_secs: u64) -> StoreWriter {
        let desc = StreamDesc {
            kind: StreamKind::Video,
            time_base: Rational(1, 10),
            params: OwnedParameters::new(codec::Parameters::new()),
        };
        Store::create(vec![desc], Duration::from_secs(window_secs)).unwrap()
    }

    #[test]
    fn decode_run_starts_at_previous_keyframe() {
        let mut w = video_store(600);
        for pts in 0..10 {
            w.append(0, pts, 1, pts % 4 == 0, &[pts as u8; 3]).unwrap();
        }
        let store = w.store().clone();
        let run: Vec<i64> = store.decode_run(0, 6).iter().map(|e| e.pts).collect();
        assert_eq!(run, vec![4, 5, 6]);
        assert_eq!(store.next_after(0, 6).unwrap().pts, 7);
        assert_eq!(store.offset_from(0, 6, -2).unwrap().pts, 4);
        assert_eq!(store.offset_from(0, 6, 100).unwrap().pts, 9, "clamped to newest");
        let mut reader = store.reader();
        assert_eq!(reader.read(&store.decode_run(0, 6)[2]).unwrap(), vec![6u8; 3]);
    }

    #[test]
    fn a_just_created_buffer_is_not_mistaken_for_an_abandoned_one() {
        // A buffer's folder exists briefly before its first segment file does.
        let dir = std::env::temp_dir().join(format!("frameback-dvr-empty-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        remove_stale_buffers();
        let survived = dir.exists();
        let _ = fs::remove_dir_all(&dir);
        assert!(survived, "a fresh, empty buffer folder was deleted");
    }

    #[test]
    fn old_segments_are_pruned_and_deleted() {
        // 10 ticks per second, keyframe every second, 2 minute window, 5 minutes of data.
        let mut w = video_store(120);
        for pts in 0..3000 {
            w.append(0, pts, 1, pts % 10 == 0, &[0; 100]).unwrap();
        }
        let store = w.store().clone();
        let (start, end) = store.video_range().unwrap();
        assert_eq!(end, 299.9);
        // Never over the window; under it by at most one keyframe interval (1 s here),
        // since the buffer must start on a keyframe.
        assert!(end - start <= 120.0 && end - start > 119.0, "{start}..{end}");
        let segments = fs::read_dir(&store.0.dir).unwrap().count();
        let expected = (120.0 / SEGMENT_SECS) as usize + 2;
        assert!(segments <= expected, "{segments} segment files left");
        assert_eq!(store.bytes(), (store.0.index.read().unwrap().streams[0].len() * 100) as u64);
    }
}
