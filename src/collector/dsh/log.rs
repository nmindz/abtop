//! Incremental reader for DSH session logs: concatenated Zstandard frames
//! (`*.jsonl.zstd`) or plaintext JSONL (`*.jsonl`).

use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// Result of one [`LogReader::read_new`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    /// Zero or more new rows were delivered.
    Appended,
    /// The file shrank or was replaced; no rows were delivered and the
    /// reader rewound to byte 0. The caller discards derived state and reads again.
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Zstd,
    Plain,
}

/// Identity of the file behind the path, so an atomic replace is noticed
/// even when the new file is not shorter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                created: meta.created().ok(),
            }
        }
    }
}

/// Bytes read from disk per step, so a first parse never buffers a whole log.
const READ_CHUNK: usize = 4 << 20;

thread_local! {
    /// One decoder per thread: frames are independent, so its window and
    /// scratch buffers are reused across logs instead of kept per log.
    static DECODER: RefCell<FrameDecoder> = RefCell::new(FrameDecoder::new());
}

pub(crate) struct LogReader {
    path: PathBuf,
    encoding: Encoding,
    /// End of the last fully consumed frame (zstd) or line (plaintext).
    offset: u64,
    identity: Option<FileIdentity>,
    /// File length at a read that could not consume everything (torn tail or
    /// corrupt structure); while the length is unchanged nothing is re-read.
    stalled_at: Option<u64>,
}

impl LogReader {
    pub(crate) fn new(path: PathBuf) -> Self {
        let encoding = if path.extension().is_some_and(|ext| ext == "zstd") {
            Encoding::Zstd
        } else {
            Encoding::Plain
        };
        Self {
            path,
            encoding,
            offset: 0,
            identity: None,
            stalled_at: None,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes the next read would consider (0 when unreadable or stalled).
    pub(crate) fn pending_bytes(&self) -> u64 {
        let Ok(len) = fs::metadata(&self.path).map(|m| m.len()) else {
            return 0;
        };
        if self.stalled_at == Some(len) {
            0
        } else {
            len.saturating_sub(self.offset)
        }
    }

    /// Deliver every complete row appended since the previous call to `on_row`,
    /// one JSON document per call, without the trailing newline.
    pub(crate) fn read_new(&mut self, on_row: &mut dyn FnMut(&[u8])) -> io::Result<ReadOutcome> {
        self.read_new_chunked(on_row, READ_CHUNK)
    }

    fn read_new_chunked(
        &mut self,
        on_row: &mut dyn FnMut(&[u8]),
        chunk: usize,
    ) -> io::Result<ReadOutcome> {
        let meta = fs::symlink_metadata(&self.path)?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(io::Error::other("session log is not a regular file"));
        }
        let identity = FileIdentity::of(&meta);
        let replaced = self.identity.is_some_and(|prev| prev != identity);
        if replaced || meta.len() < self.offset {
            self.offset = 0;
            self.stalled_at = None;
            self.identity = Some(identity);
            return Ok(ReadOutcome::Reset);
        }
        self.identity = Some(identity);
        let len = meta.len();
        if len == self.offset || self.stalled_at == Some(len) {
            return Ok(ReadOutcome::Appended);
        }

        let mut file = File::open(&self.path)?;
        // The opened file must be the one checked above (no symlink swap).
        if FileIdentity::of(&file.metadata()?) != identity {
            return Err(io::Error::other("session log changed while opening"));
        }
        file.seek(SeekFrom::Start(self.offset))?;

        let mut buf = Vec::new();
        let mut remaining = len - self.offset;
        loop {
            let want = remaining.min(chunk.max(1) as u64);
            let got = (&mut file).take(want).read_to_end(&mut buf)? as u64;
            remaining = if got == 0 { 0 } else { remaining - got };
            let (consumed, blocked) = match self.encoding {
                Encoding::Zstd => DECODER.with(|d| consume_zstd(&buf, &mut d.borrow_mut(), on_row)),
                Encoding::Plain => (consume_plain(&buf, on_row), false),
            };
            buf.drain(..consumed);
            self.offset += consumed as u64;
            if blocked || remaining == 0 {
                break;
            }
        }
        self.stalled_at = (self.offset < len).then_some(len);
        Ok(ReadOutcome::Appended)
    }
}

/// Deliver complete lines; returns bytes consumed (through the last `\n`).
fn consume_plain(buf: &[u8], on_row: &mut dyn FnMut(&[u8])) -> usize {
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return 0;
    };
    deliver_lines(&buf[..last_nl], on_row);
    last_nl + 1
}

fn deliver_lines(text: &[u8], on_row: &mut dyn FnMut(&[u8])) {
    for line in text.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.iter().all(u8::is_ascii_whitespace) {
            on_row(line);
        }
    }
}

/// Decode complete frames; returns bytes consumed and whether an invalid
/// structure blocks further progress. A torn final frame waits for more bytes.
fn consume_zstd(
    buf: &[u8],
    decoder: &mut FrameDecoder,
    on_row: &mut dyn FnMut(&[u8]),
) -> (usize, bool) {
    let mut offset = 0;
    let mut plain = Vec::new();
    while offset < buf.len() {
        match scan_frame(buf, offset) {
            FrameScan::Data(end) => {
                plain.clear();
                // A complete frame that fails to decode or verify is corrupt;
                // skip it rather than stalling every later batch.
                if decode_frame(&buf[offset..end], decoder, &mut plain).is_ok() {
                    deliver_lines(&plain, on_row);
                }
                offset = end;
            }
            FrameScan::Skippable(end) => offset = end,
            FrameScan::Torn => return (offset, false),
            FrameScan::Invalid => return (offset, true),
        }
    }
    (offset, false)
}

fn decode_frame(mut frame: &[u8], decoder: &mut FrameDecoder, out: &mut Vec<u8>) -> io::Result<()> {
    decoder.reset(&mut frame).map_err(io::Error::other)?;
    while !decoder.is_finished() {
        decoder
            .decode_blocks(&mut frame, BlockDecodingStrategy::UptoBytes(1 << 20))
            .map_err(io::Error::other)?;
        decoder.collect_to_writer(&mut *out)?;
    }
    decoder.collect_to_writer(&mut *out)?;
    let stored = decoder.get_checksum_from_data();
    if stored.is_some() && stored != decoder.get_calculated_checksum() {
        return Err(io::Error::other("zstd frame checksum mismatch"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameScan {
    /// Complete data frame ending at the given absolute offset.
    Data(usize),
    /// Complete skippable frame ending at the given absolute offset.
    Skippable(usize),
    /// EOF inside the frame.
    Torn,
    /// Not a frame; the log is corrupt from here on.
    Invalid,
}

/// Locate one frame without decompressing it (port of DSH `scanZstdFrames`).
fn scan_frame(buf: &[u8], start: usize) -> FrameScan {
    let Some(magic) = read_u32(buf, start) else {
        return FrameScan::Torn;
    };
    if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC {
        let Some(size) = read_u32(buf, start + 4) else {
            return FrameScan::Torn;
        };
        let end = start + 8 + size as usize;
        return if end <= buf.len() {
            FrameScan::Skippable(end)
        } else {
            FrameScan::Torn
        };
    }
    if magic != ZSTD_MAGIC {
        return FrameScan::Invalid;
    }

    let mut offset = start + 4;
    let Some(&descriptor) = buf.get(offset) else {
        return FrameScan::Torn;
    };
    offset += 1;
    // Reserved and unused descriptor bits, rejected like DSH does.
    if descriptor & 0x18 != 0 {
        return FrameScan::Invalid;
    }
    let content_size_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    let checksum = descriptor & 0x04 != 0;
    let dictionary_bytes = [0usize, 1, 2, 4][(descriptor & 0x03) as usize];
    let content_size_bytes = match content_size_flag {
        0 => usize::from(single_segment),
        flag => 1usize << flag,
    };
    offset += usize::from(!single_segment) + dictionary_bytes + content_size_bytes;
    if offset > buf.len() {
        return FrameScan::Torn;
    }

    loop {
        let Some(header) = buf.get(offset..offset + 3) else {
            return FrameScan::Torn;
        };
        let header = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        offset += 3;
        let last_block = header & 1 != 0;
        let block_type = (header >> 1) & 0x03;
        let block_size = (header >> 3) as usize;
        let payload = match block_type {
            1 => 1,
            3 => return FrameScan::Invalid,
            _ => block_size,
        };
        offset += payload;
        if offset > buf.len() {
            return FrameScan::Torn;
        }
        if last_block {
            break;
        }
    }

    if checksum {
        offset += 4;
        if offset > buf.len() {
            return FrameScan::Torn;
        }
    }
    FrameScan::Data(offset)
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
pub(crate) fn zstd_frame(text: &str) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn collect(reader: &mut LogReader) -> (ReadOutcome, Vec<String>) {
        let mut rows = Vec::new();
        let outcome = reader
            .read_new(&mut |row| rows.push(String::from_utf8_lossy(row).into_owned()))
            .unwrap();
        (outcome, rows)
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn zstd_reads_multiple_frames_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        append(&path, &zstd_frame("{\"a\":1}\n"));
        append(&path, &zstd_frame("{\"b\":2}\n{\"c\":3}\n"));

        let mut reader = LogReader::new(path.clone());
        let (outcome, rows) = collect(&mut reader);
        assert_eq!(outcome, ReadOutcome::Appended);
        assert_eq!(rows, vec!["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]);

        assert!(collect(&mut reader).1.is_empty());
        append(&path, &zstd_frame("{\"d\":4}\n"));
        assert_eq!(collect(&mut reader).1, vec!["{\"d\":4}"]);
    }

    #[test]
    fn zstd_torn_tail_is_read_once_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let first = zstd_frame("{\"a\":1}\n");
        let second = zstd_frame("{\"b\":2}\n");
        let (head, tail) = second.split_at(second.len() / 2);
        append(&path, &first);
        append(&path, head);

        let mut reader = LogReader::new(path.clone());
        assert_eq!(collect(&mut reader).1, vec!["{\"a\":1}"]);
        append(&path, tail);
        assert_eq!(collect(&mut reader).1, vec!["{\"b\":2}"]);
    }

    #[test]
    fn zstd_skips_skippable_frames_and_stops_at_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let mut skippable = 0x184D_2A53u32.to_le_bytes().to_vec();
        skippable.extend_from_slice(&3u32.to_le_bytes());
        skippable.extend_from_slice(b"xyz");
        append(&path, &skippable);
        append(&path, &zstd_frame("{\"a\":1}\n"));
        append(&path, b"garbage!");
        append(&path, &zstd_frame("{\"b\":2}\n"));

        let mut reader = LogReader::new(path);
        assert_eq!(collect(&mut reader).1, vec!["{\"a\":1}"]);
        assert!(collect(&mut reader).1.is_empty());
    }

    #[test]
    fn shrink_or_replace_resets_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        append(&path, &zstd_frame("{\"a\":1}\n{\"b\":2}\n"));
        let mut reader = LogReader::new(path.clone());
        assert_eq!(collect(&mut reader).1.len(), 2);

        fs::write(&path, zstd_frame("{\"z\":0}\n")).unwrap();
        let (outcome, rows) = collect(&mut reader);
        assert_eq!(outcome, ReadOutcome::Reset);
        assert!(rows.is_empty());
        assert_eq!(collect(&mut reader).1, vec!["{\"z\":0}"]);

        let replacement = dir.path().join("next.zstd");
        fs::write(
            &replacement,
            zstd_frame("{\"y\":1}\n{\"y\":2}\n{\"y\":3}\n"),
        )
        .unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert_eq!(collect(&mut reader).0, ReadOutcome::Reset);
        assert_eq!(collect(&mut reader).1.len(), 3);
    }

    #[test]
    fn corrupt_checksum_frame_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let mut bad = zstd_frame("{\"bad\":1}\n");
        assert_ne!(bad[4] & 0x04, 0, "fixture frames carry a checksum");
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        append(&path, &bad);
        append(&path, &zstd_frame("{\"ok\":1}\n"));
        let mut reader = LogReader::new(path);
        assert_eq!(collect(&mut reader).1, vec!["{\"ok\":1}"]);
    }

    #[test]
    fn small_chunks_still_decode_every_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let big: String = (0..400).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        append(&path, &zstd_frame("{\"a\":1}\n"));
        append(&path, &zstd_frame(&big));
        append(&path, &zstd_frame("{\"z\":1}\n"));
        let mut reader = LogReader::new(path);
        let mut rows = Vec::new();
        reader
            .read_new_chunked(&mut |r| rows.push(r.to_vec()), 7)
            .unwrap();
        assert_eq!(rows.len(), 402);
        assert_eq!(rows[401], b"{\"z\":1}");
    }

    #[test]
    fn stalled_tail_is_not_reread_until_it_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl.zstd");
        let frame = zstd_frame("{\"a\":1}\n");
        append(&path, &frame[..frame.len() - 2]);
        let mut reader = LogReader::new(path.clone());
        assert!(collect(&mut reader).1.is_empty());
        assert_eq!(reader.pending_bytes(), 0);
        append(&path, &frame[frame.len() - 2..]);
        assert!(reader.pending_bytes() > 0);
        assert_eq!(collect(&mut reader).1, vec!["{\"a\":1}"]);
    }

    #[test]
    fn plaintext_holds_back_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.v4.jsonl");
        append(&path, b"{\"a\":1}\r\n\n{\"b\":");
        let mut reader = LogReader::new(path.clone());
        assert_eq!(collect(&mut reader).1, vec!["{\"a\":1}"]);
        append(&path, b"2}\n");
        assert_eq!(collect(&mut reader).1, vec!["{\"b\":2}"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.jsonl");
        fs::write(&real, b"{}\n").unwrap();
        let link = dir.path().join("session.v4.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut reader = LogReader::new(link);
        assert!(reader.read_new(&mut |_| {}).is_err());
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let mut reader = LogReader::new(PathBuf::from("/nonexistent/session.v4.jsonl.zstd"));
        assert!(reader.read_new(&mut |_| {}).is_err());
    }
}
