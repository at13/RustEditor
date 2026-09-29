//! Streaming text decoding/encoding and crash-safe file writes.
//!
//! Supported encodings (no external dependency): UTF-8 (with or without
//! BOM), UTF-16 LE/BE (BOM required), and Windows-1252 (used when a file is
//! not valid UTF-8). Files are decoded chunk by chunk into a rope and
//! encoded chunk by chunk on save; the text is never materialized as one
//! string.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use ropey::{Rope, RopeBuilder};

const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Windows1252 => "Windows-1252",
        }
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Self::Utf16Le => &[0xFF, 0xFE],
            Self::Utf16Be => &[0xFE, 0xFF],
            Self::Utf8 | Self::Windows1252 => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }

    /// The platform default for new documents.
    pub fn native() -> Self {
        if cfg!(windows) { Self::CrLf } else { Self::Lf }
    }
}

/// Counts of line breaks seen while decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineEndingCounts {
    pub lf: usize,
    pub crlf: usize,
}

impl LineEndingCounts {
    /// The majority ending; `None` if the text has no line breaks.
    pub fn dominant(self) -> Option<LineEnding> {
        match (self.lf, self.crlf) {
            (0, 0) => None,
            (lf, crlf) if crlf >= lf => Some(LineEnding::CrLf),
            _ => Some(LineEnding::Lf),
        }
    }

    pub fn is_mixed(self) -> bool {
        self.lf > 0 && self.crlf > 0
    }
}

/// File identity used to detect changes made by other programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskState {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl DiskState {
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// A decoded file.
pub struct Loaded {
    pub rope: Rope,
    pub encoding: Encoding,
    pub line_endings: LineEndingCounts,
    pub disk: DiskState,
    pub read_only: bool,
}

/// Reads and decodes `path`.
pub fn load(path: &Path) -> Result<Loaded> {
    let meta = fs::metadata(path).with_context(|| format!("cannot open {}", path.display()))?;
    if meta.is_dir() {
        bail!("{} is a directory", path.display());
    }
    let disk = DiskState {
        modified: meta.modified().ok(),
        len: meta.len(),
    };
    let open = || File::open(path).with_context(|| format!("cannot open {}", path.display()));
    let (rope, encoding, line_endings) =
        decode(open()?, Some(&open)).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Loaded {
        rope,
        encoding,
        line_endings,
        disk,
        read_only: meta.permissions().readonly(),
    })
}

/// Decodes a reader. `reopen` restarts from the beginning (needed when a
/// file turns out not to be UTF-8); without it, the input is buffered.
pub fn decode<R: Read>(
    reader: R,
    reopen: Option<&dyn Fn() -> Result<File>>,
) -> Result<(Rope, Encoding, LineEndingCounts)> {
    let mut reader = BufReader::with_capacity(CHUNK, reader);
    let mut head = Vec::with_capacity(4096);
    (&mut reader).take(4096).read_to_end(&mut head)?;
    if head.starts_with(&[0xFF, 0xFE]) {
        let rest = head[2..].to_vec();
        return decode_utf16(rest.chain(reader), false).map(|(r, c)| (r, Encoding::Utf16Le, c));
    }
    if head.starts_with(&[0xFE, 0xFF]) {
        let rest = head[2..].to_vec();
        return decode_utf16(rest.chain(reader), true).map(|(r, c)| (r, Encoding::Utf16Be, c));
    }
    let bom = head.starts_with(&[0xEF, 0xBB, 0xBF]);
    if head.contains(&0) {
        bail!("this looks like a binary file (it contains NUL bytes)");
    }
    let start = if bom { 3 } else { 0 };
    let mut buffered = Vec::new();
    let keep = reopen.is_none();
    let first = head[start..].to_vec();
    match decode_utf8(first.chain(reader), keep.then_some(&mut buffered)) {
        Ok((rope, counts)) => Ok((
            rope,
            if bom {
                Encoding::Utf8Bom
            } else {
                Encoding::Utf8
            },
            counts,
        )),
        Err(Utf8Failure::Io(error)) => Err(error.into()),
        Err(Utf8Failure::Invalid) => {
            // Not UTF-8: decode the whole input as Windows-1252.
            let (rope, counts) = match reopen {
                Some(reopen) => {
                    let mut file = reopen()?;
                    if bom {
                        let mut skip = [0u8; 3];
                        file.read_exact(&mut skip)?;
                    }
                    decode_1252(file)?
                }
                None => {
                    // `buffered` holds everything after the BOM.
                    decode_1252(&buffered[..])?
                }
            };
            Ok((rope, Encoding::Windows1252, counts))
        }
    }
}

enum Utf8Failure {
    Invalid,
    Io(io::Error),
}

fn count_ascii_breaks(bytes: &[u8], pending_cr: &mut bool, counts: &mut LineEndingCounts) {
    for &byte in bytes {
        match byte {
            b'\n' if *pending_cr => counts.crlf += 1,
            b'\n' => counts.lf += 1,
            _ => {}
        }
        *pending_cr = byte == b'\r';
    }
}

fn decode_utf8<R: Read>(
    mut reader: R,
    mut keep: Option<&mut Vec<u8>>,
) -> Result<(Rope, LineEndingCounts), Utf8Failure> {
    let mut builder = RopeBuilder::new();
    let mut counts = LineEndingCounts::default();
    let mut pending_cr = false;
    let mut buf = vec![0u8; CHUNK];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let read = reader.read(&mut buf).map_err(Utf8Failure::Io)?;
        if let Some(keep) = keep.as_deref_mut() {
            keep.extend_from_slice(&buf[..read]);
        }
        if read == 0 {
            if !carry.is_empty() {
                return Err(Utf8Failure::Invalid);
            }
            break;
        }
        carry.extend_from_slice(&buf[..read]);
        let valid = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(Utf8Failure::Invalid),
        };
        let text = std::str::from_utf8(&carry[..valid]).expect("validated");
        count_ascii_breaks(text.as_bytes(), &mut pending_cr, &mut counts);
        builder.append(text);
        carry.drain(..valid);
    }
    Ok((builder.finish(), counts))
}

/// Windows-1252 code points for bytes 0x80..=0x9F (undefined bytes map to
/// the matching C1 control, as browsers do).
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}', '\u{8F}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}', '\u{178}',
];

fn decode_1252<R: Read>(mut reader: R) -> Result<(Rope, LineEndingCounts)> {
    let mut builder = RopeBuilder::new();
    let mut counts = LineEndingCounts::default();
    let mut pending_cr = false;
    let mut buf = vec![0u8; CHUNK];
    let mut text = String::with_capacity(CHUNK * 2);
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        count_ascii_breaks(&buf[..read], &mut pending_cr, &mut counts);
        text.clear();
        text.extend(buf[..read].iter().map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
            _ => b as char,
        }));
        builder.append(&text);
    }
    Ok((builder.finish(), counts))
}

fn decode_utf16<R: Read>(mut reader: R, big_endian: bool) -> Result<(Rope, LineEndingCounts)> {
    let mut builder = RopeBuilder::new();
    let mut counts = LineEndingCounts::default();
    let mut pending_cr = false;
    let mut buf = vec![0u8; CHUNK];
    let mut carry: Vec<u8> = Vec::new();
    let mut high: Option<u16> = None;
    let mut text = String::with_capacity(CHUNK);
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            if !carry.is_empty() || high.is_some() {
                bail!("the file ends in the middle of a UTF-16 character");
            }
            break;
        }
        carry.extend_from_slice(&buf[..read]);
        let whole = carry.len() / 2 * 2;
        let mut units: Vec<u16> = carry[..whole]
            .chunks_exact(2)
            .map(|pair| {
                if big_endian {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        carry.drain(..whole);
        if let Some(h) = high.take() {
            units.insert(0, h);
        }
        // Keep a trailing high surrogate for the next chunk.
        if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            high = units.pop();
        }
        text.clear();
        for ch in char::decode_utf16(units) {
            let ch = ch.map_err(|_| anyhow::anyhow!("invalid UTF-16 text"))?;
            match ch {
                '\n' if pending_cr => counts.crlf += 1,
                '\n' => counts.lf += 1,
                _ => {}
            }
            pending_cr = ch == '\r';
            text.push(ch);
        }
        builder.append(&text);
    }
    Ok((builder.finish(), counts))
}

/// Writes `rope` in `encoding`. Fails (before writing anything past the
/// problem) if a character cannot be represented.
pub fn encode(rope: &Rope, encoding: Encoding, out: &mut (impl Write + ?Sized)) -> Result<()> {
    out.write_all(encoding.bom())?;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK * 2);
    for chunk in rope.chunks() {
        match encoding {
            Encoding::Utf8 | Encoding::Utf8Bom => out.write_all(chunk.as_bytes())?,
            Encoding::Utf16Le | Encoding::Utf16Be => {
                buf.clear();
                for unit in chunk.encode_utf16() {
                    buf.extend_from_slice(&if encoding == Encoding::Utf16Le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                out.write_all(&buf)?;
            }
            Encoding::Windows1252 => {
                buf.clear();
                for ch in chunk.chars() {
                    buf.push(encode_1252(ch).with_context(|| {
                        format!(
                            "'{ch}' (U+{:04X}) cannot be saved as Windows-1252; \
                             change the encoding to UTF-8",
                            ch as u32
                        )
                    })?);
                }
                out.write_all(&buf)?;
            }
        }
    }
    Ok(())
}

fn encode_1252(ch: char) -> Option<u8> {
    let code = ch as u32;
    if code < 0x80 || (0xA0..=0xFF).contains(&code) {
        return Some(code as u8);
    }
    CP1252_HIGH
        .iter()
        .position(|&c| c == ch)
        .map(|i| 0x80 + i as u8)
}

/// Whether [`atomic_write`] replaced the file atomically or had to write in
/// place (e.g. another program holds the file open on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Atomic,
    InPlace,
}

/// Writes a file crash-safely: the content goes to a temporary file in the
/// same directory, is flushed to disk, and then renamed over `target`, so a
/// crash leaves either the old or the new file. Symlinks are followed (the
/// link target is replaced) and existing permissions are kept. If the rename
/// is refused, the content is written in place instead.
pub fn atomic_write(
    target: &Path,
    write: impl Fn(&mut dyn Write) -> Result<()>,
) -> Result<WriteMode> {
    let target = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = target
        .file_name()
        .context("the path has no file name")?
        .to_string_lossy()
        .into_owned();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp = dir.join(format!(".{name}.{}-{nonce}.tmp", std::process::id()));
    let existing = fs::metadata(&target).ok();

    let file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
        Ok(file) => file,
        // The directory is not writable but the file may be: write in place.
        Err(create_error) if existing.is_some() => {
            return write_in_place(&target, &write).map_err(|error| {
                error.context(format!("cannot create a temporary file: {create_error}"))
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot create {}", target.display()));
        }
    };
    let written = (|| -> Result<()> {
        let mut out = BufWriter::with_capacity(CHUNK, file);
        write(&mut out)?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&temp, meta.permissions())?;
        }
        Ok(())
    })();
    if let Err(error) = written {
        // The original is untouched; never fall back to writing in place
        // here, since the same failure would truncate it.
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    match fs::rename(&temp, &target) {
        Ok(()) => {
            sync_dir(&dir);
            Ok(WriteMode::Atomic)
        }
        Err(rename_error) => {
            // E.g. another program holds the file open without delete
            // sharing on Windows. The content already encoded once, so an
            // in-place write can only fail on I/O.
            let _ = fs::remove_file(&temp);
            write_in_place(&target, &write)
                .map_err(|error| error.context(format!("rename failed: {rename_error}")))
        }
    }
}

fn write_in_place(
    target: &Path,
    write: &dyn Fn(&mut dyn Write) -> Result<()>,
) -> Result<WriteMode> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)
        .with_context(|| format!("cannot write {}", target.display()))?;
    let mut out = BufWriter::with_capacity(CHUNK, file);
    write(&mut out)?;
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(WriteMode::InPlace)
}

/// Persists the rename on Unix (no-op elsewhere).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Rewrites every line break (`\n`, `\r\n`) as `ending`, streaming.
pub fn convert_line_endings(rope: &Rope, ending: LineEnding) -> Rope {
    let mut builder = RopeBuilder::new();
    let mut out = String::with_capacity(CHUNK);
    let mut pending_cr = false;
    for chunk in rope.chunks() {
        out.clear();
        for ch in chunk.chars() {
            if pending_cr {
                pending_cr = false;
                if ch == '\n' {
                    out.push_str(ending.as_str());
                    continue;
                }
                out.push('\r');
            }
            match ch {
                '\r' => pending_cr = true,
                '\n' => out.push_str(ending.as_str()),
                _ => out.push(ch),
            }
        }
        builder.append(&out);
    }
    if pending_cr {
        builder.append("\r");
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rusteditor-io-{name}-{nonce}.txt"))
    }

    fn roundtrip(text: &str, encoding: Encoding) -> Vec<u8> {
        let rope = Rope::from_str(text);
        let mut bytes = Vec::new();
        encode(&rope, encoding, &mut bytes).unwrap();
        let (decoded, detected, _) = decode(&bytes[..], None).unwrap();
        assert_eq!(decoded.to_string(), text, "{encoding:?}");
        assert_eq!(detected, encoding);
        bytes
    }

    #[test]
    fn encodings_round_trip_with_detection() {
        let text = "héllo 😀 wörld\r\nline 2\n";
        roundtrip(text, Encoding::Utf8);
        assert_eq!(
            &roundtrip(text, Encoding::Utf8Bom)[..3],
            &[0xEF, 0xBB, 0xBF]
        );
        assert_eq!(&roundtrip(text, Encoding::Utf16Le)[..2], &[0xFF, 0xFE]);
        assert_eq!(&roundtrip(text, Encoding::Utf16Be)[..2], &[0xFE, 0xFF]);
        roundtrip(
            "caf\u{e9} \u{20ac}5 \u{201c}quoted\u{201d}\n",
            Encoding::Windows1252,
        );
    }

    #[test]
    fn invalid_utf8_falls_back_to_windows_1252() {
        let bytes = b"caf\xe9 \x80 ok\r\n";
        let (rope, encoding, counts) = decode(&bytes[..], None).unwrap();
        assert_eq!(encoding, Encoding::Windows1252);
        assert_eq!(rope.to_string(), "caf\u{e9} \u{20ac} ok\r\n");
        assert_eq!(counts, LineEndingCounts { lf: 0, crlf: 1 });
        // Invalid bytes far beyond the first chunk also switch encodings.
        let mut big = "a".repeat(CHUNK * 3).into_bytes();
        big.push(0xE9);
        let (rope, encoding, _) = decode(&big[..], None).unwrap();
        assert_eq!(encoding, Encoding::Windows1252);
        assert_eq!(rope.len_chars(), CHUNK * 3 + 1);
    }

    #[test]
    fn multibyte_characters_split_across_chunks_decode() {
        let text = "é".repeat(CHUNK) + &"😀".repeat(CHUNK / 3);
        let (rope, encoding, _) = decode(text.as_bytes(), None).unwrap();
        assert_eq!(encoding, Encoding::Utf8);
        assert_eq!(rope.to_string(), text);
        let mut utf16 = vec![0xFF, 0xFE, 0x41]; // odd byte count mid-stream
        utf16.pop();
        for unit in "😀x".repeat(CHUNK / 3).encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let (rope, _, _) = decode(&utf16[..], None).unwrap();
        assert_eq!(rope.to_string(), "😀x".repeat(CHUNK / 3));
    }

    #[test]
    fn detects_line_endings_and_rejects_binary() {
        let (_, _, counts) = decode(&b"a\r\nb\r\nc\nd"[..], None).unwrap();
        assert_eq!(counts, LineEndingCounts { lf: 1, crlf: 2 });
        assert_eq!(counts.dominant(), Some(LineEnding::CrLf));
        assert!(counts.is_mixed());
        assert_eq!(LineEndingCounts::default().dominant(), None);
        assert!(decode(&b"ELF\0\x01\x02"[..], None).is_err());
        assert!(decode(&[0xFF, 0xFE, 0x3D, 0xD8][..], None).is_err()); // lone surrogate
    }

    #[test]
    fn unrepresentable_characters_fail_windows_1252_encoding() {
        let mut out = Vec::new();
        let error = encode(&Rope::from_str("ok 😀"), Encoding::Windows1252, &mut out)
            .unwrap_err()
            .to_string();
        assert!(error.contains("U+1F600"), "{error}");
    }

    #[test]
    fn converts_line_endings_including_split_pairs() {
        let mut text = "a".repeat(CHUNK - 1);
        text.push_str("\r\nb\nc\r\n\r");
        let rope = Rope::from_str(&text);
        assert!(rope.chunks().count() > 1);
        let lf = convert_line_endings(&rope, LineEnding::Lf).to_string();
        assert_eq!(&lf[CHUNK - 1..], "\nb\nc\n\r");
        let crlf = convert_line_endings(&Rope::from_str("x\ny\r\nz"), LineEnding::CrLf);
        assert_eq!(crlf.to_string(), "x\r\ny\r\nz");
    }

    #[test]
    fn atomic_write_replaces_and_keeps_permissions() {
        let path = temp_path("atomic");
        fs::write(&path, "old").unwrap();
        let before = fs::metadata(&path).unwrap().permissions();
        let mode = atomic_write(&path, |out| {
            out.write_all(b"new content")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        assert_eq!(mode, WriteMode::Atomic);
        assert_eq!(fs::metadata(&path).unwrap().permissions(), before);
        // No temporary files are left behind.
        let dir = path.parent().unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let leftovers = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!(".{name}."))
            })
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_writes_leave_the_original_untouched() {
        let path = temp_path("fail");
        fs::write(&path, "precious").unwrap();
        let result = atomic_write(&path, |out| {
            out.write_all(b"partial")?;
            bail!("encoding failed halfway")
        });
        assert!(result.is_err());
        // The original is untouched and no temporary file is left.
        assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!(".{name}."))
            })
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_reads_disk_state_and_encoding() {
        let path = temp_path("load");
        fs::write(&path, b"\xEF\xBB\xBFhello\r\nworld").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.encoding, Encoding::Utf8Bom);
        assert_eq!(loaded.rope.to_string(), "hello\r\nworld");
        assert_eq!(loaded.line_endings.dominant(), Some(LineEnding::CrLf));
        assert_eq!(loaded.disk, DiskState::of(&path).unwrap());
        assert!(load(path.parent().unwrap()).is_err());
        // Invalid UTF-8 after a BOM re-reads the file as Windows-1252.
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend("x".repeat(CHUNK * 2).bytes());
        bytes.extend_from_slice(b"caf\xe9");
        fs::write(&path, &bytes).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.encoding, Encoding::Windows1252);
        assert!(loaded.rope.to_string().ends_with("xcaf\u{e9}"));
        assert_eq!(loaded.rope.len_chars(), CHUNK * 2 + 4);
        fs::remove_file(path).unwrap();
    }
}
