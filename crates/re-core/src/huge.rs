//! Huge files: read-only access without loading the file into memory.
//!
//! - Bytes are read at positions (`seek_read` / `read_at`) through a cache
//!   of 64 KiB chunks (64 MiB total, oldest evicted first), so memory is
//!   bounded whatever the file size.
//! - [`LineIndex`] stores the byte offset of every 1,024th line (about 8
//!   bytes per 1,024 lines: ~160 KB for 20M lines). It is built by one
//!   sequential scan ([`build_index`]) that reports progress, so the first
//!   lines are viewable immediately.
//! - A line is found from the nearest sample by counting line breaks; a
//!   small cache of recent line starts makes scrolling O(line length).
//! - [`Lines`] streams every line for search, with a cap per line.
//!
//! Text is decoded as UTF-8 (invalid bytes shown as U+FFFD); `\n` and
//! `\r\n` end lines. UTF-16 files are refused (their line breaks are two
//! bytes). The file is kept open without a lock (other programs can still
//! write, rename or delete it); callers compare [`HugeFile::disk`] with the
//! file on disk to notice changes.

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};

use crate::text_io::DiskState;

/// Files at least this large open read-only in huge-file mode.
pub const HUGE_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// A line start is sampled every this many lines.
pub const SAMPLE_EVERY: usize = 1024;
/// Cache geometry: 1,024 chunks of 64 KiB (64 MiB). Small chunks keep a
/// random jump cheap (one or two 64 KiB reads instead of 1 MiB).
const CHUNK_BYTES: usize = 64 * 1024;
const CACHED_CHUNKS: usize = 1024;
/// Sequential scans (index, search) read this much at a time.
const SCAN_BYTES: usize = 1024 * 1024;
const LINE_START_CACHE: usize = 4096;
/// Progress is reported at least every this many bytes while indexing.
const PROGRESS_BYTES: u64 = 64 * 1024 * 1024;

/// Positional read that does not move a shared cursor.
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

/// Reads until `buf` is full or end of file. Returns bytes read.
fn read_full_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match read_at(file, &mut buf[filled..], offset) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                offset += n as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Sampled line starts, possibly still being built.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineIndex {
    /// `samples[k]` is the byte offset where line `k * every` starts.
    samples: Vec<u64>,
    every: usize,
    /// Lines known so far (the last may still grow while indexing).
    lines: usize,
    /// Bytes scanned so far.
    scanned: u64,
    complete: bool,
}

impl LineIndex {
    pub fn lines(&self) -> usize {
        self.lines
    }

    pub fn scanned_bytes(&self) -> u64 {
        self.scanned
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn memory_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<u64>()
    }
}

struct Chunks {
    map: HashMap<u64, Arc<[u8]>>,
    order: VecDeque<u64>,
}

/// A huge file opened for reading.
pub struct HugeFile {
    path: PathBuf,
    file: File,
    len: u64,
    disk: DiskState,
    chunk_bytes: usize,
    cached_chunks: usize,
    chunks: Mutex<Chunks>,
    /// Recently resolved `line -> byte offset`.
    line_starts: Mutex<HashMap<usize, u64>>,
}

impl std::fmt::Debug for HugeFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HugeFile")
            .field("path", &self.path)
            .field("len", &self.len)
            .finish()
    }
}

impl HugeFile {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, CHUNK_BYTES, CACHED_CHUNKS)
    }

    /// `open` with explicit cache geometry (tests use tiny chunks).
    pub fn open_with(path: &Path, chunk_bytes: usize, cached_chunks: usize) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let disk = DiskState::of(path)?;
        let mut head = [0u8; 2];
        let read = read_full_at(&file, &mut head, 0)?;
        if read == 2 && (head == [0xFF, 0xFE] || head == [0xFE, 0xFF]) {
            bail!("UTF-16 files this large are not supported yet");
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: disk.len,
            disk,
            chunk_bytes: chunk_bytes.max(16),
            cached_chunks: cached_chunks.max(2),
            chunks: Mutex::new(Chunks {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            line_starts: Mutex::new(HashMap::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The file's state when opened (compare with `DiskState::of`).
    pub fn disk(&self) -> DiskState {
        self.disk
    }

    fn chunk(&self, index: u64) -> io::Result<Arc<[u8]>> {
        if let Some(chunk) = self.chunks.lock().expect("cache").map.get(&index) {
            return Ok(chunk.clone());
        }
        let mut buf = vec![0u8; self.chunk_bytes];
        let read = read_full_at(&self.file, &mut buf, index * self.chunk_bytes as u64)?;
        buf.truncate(read);
        let chunk: Arc<[u8]> = buf.into();
        let mut cache = self.chunks.lock().expect("cache");
        if cache.map.insert(index, chunk.clone()).is_none() {
            cache.order.push_back(index);
        }
        while cache.order.len() > self.cached_chunks {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        Ok(chunk)
    }

    /// Byte offset just after the `count`-th line break at or after `from`,
    /// or `None` at end of file.
    fn skip_lines(&self, mut from: u64, mut count: usize) -> io::Result<Option<u64>> {
        while count > 0 {
            if from >= self.len {
                return Ok(None);
            }
            let index = from / self.chunk_bytes as u64;
            let chunk = self.chunk(index)?;
            let start = (from - index * self.chunk_bytes as u64) as usize;
            if start >= chunk.len() {
                return Ok(None); // file shrank
            }
            for position in memchr::memchr_iter(b'\n', &chunk[start..]) {
                count -= 1;
                if count == 0 {
                    let after = start + position + 1;
                    return Ok(Some(index * self.chunk_bytes as u64 + after as u64));
                }
            }
            from = index * self.chunk_bytes as u64 + chunk.len() as u64;
        }
        Ok(Some(from))
    }

    /// Byte offset where `line` starts.
    fn line_start(&self, index: &LineIndex, line: usize) -> io::Result<Option<u64>> {
        if line >= index.lines {
            return Ok(None);
        }
        {
            let starts = self.line_starts.lock().expect("line starts");
            if let Some(&start) = starts.get(&line) {
                return Ok(Some(start));
            }
            // Scrolling: continue from the previous line if known.
            if let Some(&previous) = line.checked_sub(1).and_then(|l| starts.get(&l)) {
                drop(starts);
                let start = self.skip_lines(previous, 1)?;
                return Ok(start.inspect(|&s| self.remember(line, s)));
            }
        }
        let sample = line / index.every;
        let Some(&from) = index.samples.get(sample) else {
            return Ok(None);
        };
        let start = self.skip_lines(from, line - sample * index.every)?;
        Ok(start.inspect(|&s| self.remember(line, s)))
    }

    fn remember(&self, line: usize, start: u64) {
        let mut starts = self.line_starts.lock().expect("line starts");
        if starts.len() >= LINE_START_CACHE {
            starts.clear();
        }
        starts.insert(line, start);
    }

    /// Up to `max_bytes` of `line` without its line break, decoded lossily.
    /// Returns the text and whether it was cut short.
    pub fn line_text(
        &self,
        index: &LineIndex,
        line: usize,
        max_bytes: usize,
    ) -> io::Result<Option<(String, bool)>> {
        let Some(start) = self.line_start(index, line)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        let mut from = start;
        let mut truncated = false;
        loop {
            if from >= self.len {
                break;
            }
            let chunk_index = from / self.chunk_bytes as u64;
            let chunk = self.chunk(chunk_index)?;
            let offset = (from - chunk_index * self.chunk_bytes as u64) as usize;
            if offset >= chunk.len() {
                break;
            }
            let rest = &chunk[offset..];
            let (piece, done) = match memchr::memchr(b'\n', rest) {
                Some(end) => (&rest[..end], true),
                None => (rest, false),
            };
            let room = max_bytes.saturating_sub(bytes.len());
            if piece.len() > room {
                bytes.extend_from_slice(&piece[..room]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(piece);
            if done {
                break;
            }
            from += rest.len() as u64;
        }
        if !truncated && bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            // A cut may split a character; drop a trailing replacement.
            while text.ends_with('\u{FFFD}') {
                text.pop();
            }
        }
        Ok(Some((text, truncated)))
    }

    /// Streams every line (for search). See [`Lines`].
    pub fn lines(path: &Path, max_line_bytes: usize) -> Result<Lines> {
        Ok(Lines {
            file: File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
            buf: vec![0; SCAN_BYTES],
            pos: 0,
            end: 0,
            offset: 0,
            line: Vec::new(),
            over: false,
            number: 0,
            max_line_bytes,
            finished: false,
        })
    }
}

/// Scans `path` and builds its [`LineIndex`], calling `progress` with
/// partial indexes (at least every 64 MiB) and the final one. Stops early
/// (returning `None`) if `cancel` is set.
pub fn build_index(
    path: &Path,
    every: usize,
    cancel: Option<&AtomicBool>,
    mut progress: impl FnMut(&LineIndex),
) -> Result<Option<LineIndex>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let every = every.max(1);
    let mut index = LineIndex {
        samples: vec![0],
        every,
        lines: 1,
        scanned: 0,
        complete: false,
    };
    let mut buf = vec![0u8; SCAN_BYTES];
    let mut offset = 0u64;
    let mut next_report = PROGRESS_BYTES;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(None);
        }
        let read = read_full_at(&file, &mut buf, offset)?;
        if read == 0 {
            break;
        }
        for position in memchr::memchr_iter(b'\n', &buf[..read]) {
            // A new line starts after every line break (like ropey, a
            // trailing break yields a final empty line).
            if index.lines.is_multiple_of(every) {
                index.samples.push(offset + position as u64 + 1);
            }
            index.lines += 1;
        }
        offset += read as u64;
        index.scanned = offset;
        if offset >= next_report {
            progress(&index);
            next_report = offset + PROGRESS_BYTES;
        }
        if read < buf.len() {
            break;
        }
    }
    index.complete = true;
    progress(&index);
    Ok(Some(index))
}

/// Iterates `(line number, text)` over a file, decoding lossily and cutting
/// each line at `max_line_bytes` (the rest of a longer line is skipped).
pub struct Lines {
    file: File,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    offset: u64,
    line: Vec<u8>,
    over: bool,
    number: usize,
    max_line_bytes: usize,
    finished: bool,
}

impl Lines {
    fn take_line(&mut self) -> (usize, String) {
        if !self.over && self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        self.over = false;
        let number = self.number;
        self.number += 1;
        (number, text)
    }
}

impl Iterator for Lines {
    type Item = io::Result<(usize, String)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }
            if self.pos == self.end {
                match read_full_at(&self.file, &mut self.buf, self.offset) {
                    Ok(0) => {
                        self.finished = true;
                        return Some(Ok(self.take_line()));
                    }
                    Ok(n) => {
                        self.pos = 0;
                        self.end = n;
                        self.offset += n as u64;
                    }
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
            }
            let rest = &self.buf[self.pos..self.end];
            let (piece, done) = match memchr::memchr(b'\n', rest) {
                Some(end) => (&rest[..end], true),
                None => (rest, false),
            };
            let room = self.max_line_bytes.saturating_sub(self.line.len());
            if piece.len() > room {
                self.line.extend_from_slice(&piece[..room]);
                self.over = true;
            } else if !self.over {
                self.line.extend_from_slice(piece);
            }
            self.pos += piece.len() + usize::from(done);
            if done {
                return Some(Ok(self.take_line()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rusteditor-huge-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn sample_text() -> String {
        let mut text = String::new();
        for i in 0..200 {
            match i % 7 {
                0 => text.push_str(&format!("line {i} with é and 😀\r\n")),
                1 => text.push('\n'),
                2 => text.push_str(&"x".repeat(i * 3 + 1)),
                3 => text.push_str(&format!("{i}\n")),
                _ => text.push_str(&format!("日本語 {i}\n")),
            }
            if i % 7 == 2 {
                text.push('\n');
            }
        }
        text
    }

    #[test]
    fn lines_match_a_rope_for_every_chunk_and_sample_size() {
        for trailing in [true, false] {
            let mut text = sample_text();
            if !trailing {
                text.push_str("last without newline");
            }
            let rope = ropey::Rope::from_str(&text);
            let path = temp("lines", text.as_bytes());
            for (chunk, every) in [(16, 1), (17, 3), (64, 5), (1 << 20, 1024)] {
                let file = HugeFile::open_with(&path, chunk, 3).unwrap();
                let index = build_index(&path, every, None, |_| {}).unwrap().unwrap();
                assert!(index.is_complete());
                assert_eq!(
                    index.lines(),
                    rope.len_lines(),
                    "chunk {chunk} every {every}"
                );
                // Random access, then sequential (exercises both caches).
                let order: Vec<usize> = (0..rope.len_lines())
                    .rev()
                    .step_by(3)
                    .chain(0..rope.len_lines())
                    .collect();
                for line in order {
                    let expected = rope.line(line).to_string();
                    let expected = expected.trim_end_matches('\n').trim_end_matches('\r');
                    let (got, cut) = file.line_text(&index, line, usize::MAX).unwrap().unwrap();
                    assert_eq!(got, expected, "line {line} chunk {chunk} every {every}");
                    assert!(!cut);
                }
                assert!(
                    file.line_text(&index, rope.len_lines(), 10)
                        .unwrap()
                        .is_none()
                );
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn long_lines_are_cut_without_splitting_characters() {
        let text = format!("{}\nshort", "é".repeat(100));
        let path = temp("cut", text.as_bytes());
        let file = HugeFile::open_with(&path, 16, 2).unwrap();
        let index = build_index(&path, 2, None, |_| {}).unwrap().unwrap();
        let (got, cut) = file.line_text(&index, 0, 11).unwrap().unwrap();
        assert!(cut);
        assert_eq!(got, "é".repeat(5)); // 10 bytes; the 11th was half a char
        assert_eq!(
            file.line_text(&index, 1, 11).unwrap().unwrap(),
            ("short".into(), false)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn progress_is_reported_and_indexing_can_be_cancelled() {
        let path = temp("progress", b"a\nb\nc\n");
        let mut reports = Vec::new();
        let index = build_index(&path, 1, None, |i| reports.push(i.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(reports.last(), Some(&index));
        assert_eq!(index.lines(), 4);
        assert_eq!(index.scanned_bytes(), 6);
        let cancel = AtomicBool::new(true);
        assert!(
            build_index(&path, 1, Some(&cancel), |_| {})
                .unwrap()
                .is_none()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn streaming_lines_match_and_cap_long_lines() {
        let mut text = sample_text();
        text.push_str("tail");
        let rope = ropey::Rope::from_str(&text);
        let path = temp("stream", text.as_bytes());
        let lines: Vec<(usize, String)> = HugeFile::lines(&path, 1 << 20)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(lines.len(), rope.len_lines());
        for (number, line) in &lines {
            let expected = rope.line(*number).to_string();
            assert_eq!(line, expected.trim_end_matches('\n').trim_end_matches('\r'));
        }
        let capped: Vec<_> = HugeFile::lines(&path, 4)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(capped.len(), rope.len_lines());
        assert!(
            capped
                .iter()
                .all(|(_, l)| l.len() <= 4 || l.contains('\u{FFFD}'))
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn utf16_is_refused_and_disk_state_is_recorded() {
        let path = temp("utf16", &[0xFF, 0xFE, b'a', 0]);
        assert!(
            HugeFile::open(&path)
                .unwrap_err()
                .to_string()
                .contains("UTF-16")
        );
        std::fs::write(&path, b"abc").unwrap();
        let file = HugeFile::open(&path).unwrap();
        assert_eq!(file.len(), 3);
        assert_eq!(file.disk(), DiskState::of(&path).unwrap());
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    /// Manual benchmark: `cargo test -p re-core --release huge_file_benchmark
    /// -- --ignored --nocapture` (writes and deletes a 1 GiB temp file).
    #[test]
    #[ignore = "writes a 1 GiB file; run manually in release"]
    fn huge_file_benchmark() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("rusteditor-huge-bench-{}", std::process::id()));
        {
            let mut out = std::io::BufWriter::new(File::create(&path).unwrap());
            let mut written = 0u64;
            let mut n = 0u64;
            while written < 1 << 30 {
                let line = format!(
                    "2025-01-01T00:00:{n:08} INFO request id={n} status=200 took={}ms\n",
                    n % 997
                );
                out.write_all(line.as_bytes()).unwrap();
                written += line.len() as u64;
                n += 1;
            }
        }
        let started = std::time::Instant::now();
        let file = HugeFile::open(&path).unwrap();
        let open = started.elapsed();
        let started = std::time::Instant::now();
        let index = build_index(&path, SAMPLE_EVERY, None, |_| {})
            .unwrap()
            .unwrap();
        let indexing = started.elapsed();
        let started = std::time::Instant::now();
        let mut total = 0;
        for k in 0..1000 {
            let line = (k * 7919 * 1013) % index.lines();
            total += file.line_text(&index, line, 1024).unwrap().unwrap().0.len();
        }
        let random = started.elapsed();
        let started = std::time::Instant::now();
        for line in 5_000_000..5_000_040 {
            total += file.line_text(&index, line, 1024).unwrap().unwrap().0.len();
        }
        let screen = started.elapsed();
        eprintln!(
            "BENCH {:.2} GiB, {} lines: open {open:?}, index {indexing:?} ({:.0} MiB/s), \
             index memory {} KiB, 1000 random lines {random:?}, one screen (40 lines) {screen:?} [{total}]",
            file.len() as f64 / (1u64 << 30) as f64,
            index.lines(),
            file.len() as f64 / (1 << 20) as f64 / indexing.as_secs_f64(),
            index.memory_bytes() / 1024,
        );
        drop(file);
        std::fs::remove_file(path).unwrap();
    }
}
