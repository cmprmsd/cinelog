//! zstd support: magic detection, multi-frame writing cut at newlines, frame tables for
//! seeking, and atomic compression of finished casts (§6.6).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use memchr::{memchr, memrchr};
use zstd::zstd_safe::CParameter;

use crate::CastError;

/// Little-endian magic of a zstd frame.
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// Uncompressed size of one frame (frames are cut at the last newline before it).
pub const DEFAULT_FRAME_BYTES: usize = 4 << 20;
/// Compression level for finished casts.
pub const DEFAULT_LEVEL: i32 = 9;

const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// True if `bytes` starts with a zstd frame or a skippable frame.
pub fn is_zstd_magic(bytes: &[u8]) -> bool {
    match bytes.first_chunk::<4>() {
        Some(m) => *m == ZSTD_MAGIC || u32::from_le_bytes(*m) & SKIPPABLE_MASK == SKIPPABLE_MAGIC,
        None => false,
    }
}

/// Checks the file's magic without moving its cursor.
pub(crate) fn sniff(file: &File) -> io::Result<bool> {
    let mut magic = [0u8; 4];
    let mut got = 0;
    while got < magic.len() {
        match file.read_at(&mut magic[got..], got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(is_zstd_magic(&magic[..got]))
}

/// One independent frame: compressed and uncompressed offset and length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZstFrame {
    pub c_offset: u64,
    pub c_len: u64,
    pub u_offset: u64,
    pub u_len: u64,
}

/// Writes independent zstd frames of about `frame_bytes` uncompressed bytes, each ending at
/// a newline (a line longer than four frames is cut at four frames), with content size and
/// checksum in every frame header.
pub struct FrameEncoder<W: Write> {
    inner: W,
    comp: zstd::bulk::Compressor<'static>,
    frame_bytes: usize,
    buf: Vec<u8>,
    /// `buf[..newline_free]` is known to hold no newline, so writes of a long line do not
    /// rescan the whole buffer.
    newline_free: usize,
    frames: Vec<ZstFrame>,
    c_off: u64,
    u_off: u64,
}

impl<W: Write> FrameEncoder<W> {
    pub fn new(inner: W, level: i32) -> io::Result<Self> {
        Self::with_frame_bytes(inner, level, DEFAULT_FRAME_BYTES)
    }

    pub fn with_frame_bytes(inner: W, level: i32, frame_bytes: usize) -> io::Result<Self> {
        let mut comp = zstd::bulk::Compressor::new(level)?;
        comp.set_parameter(CParameter::ChecksumFlag(true))?;
        comp.set_parameter(CParameter::ContentSizeFlag(true))?;
        let frame_bytes = frame_bytes.max(1);
        Ok(Self {
            inner,
            comp,
            frame_bytes,
            buf: Vec::with_capacity(frame_bytes + 64 * 1024),
            newline_free: 0,
            frames: Vec::new(),
            c_off: 0,
            u_off: 0,
        })
    }

    /// Frames completed so far.
    pub fn frames(&self) -> &[ZstFrame] {
        &self.frames
    }

    /// Compresses the remaining data and returns the writer and the frame table.
    pub fn finish(mut self) -> io::Result<(W, Vec<ZstFrame>)> {
        if !self.buf.is_empty() {
            self.emit(self.buf.len())?;
        }
        self.inner.flush()?;
        Ok((self.inner, self.frames))
    }

    fn emit(&mut self, n: usize) -> io::Result<()> {
        let compressed = self.comp.compress(&self.buf[..n])?;
        self.inner.write_all(&compressed)?;
        self.frames.push(ZstFrame {
            c_offset: self.c_off,
            c_len: compressed.len() as u64,
            u_offset: self.u_off,
            u_len: n as u64,
        });
        self.c_off += compressed.len() as u64;
        self.u_off += n as u64;
        self.buf.drain(..n);
        self.newline_free = self.newline_free.saturating_sub(n);
        Ok(())
    }

    /// Where to end the next frame: after the last newline within `frame_bytes`, else
    /// after the first one beyond, else at `4 * frame_bytes` (a newline-free run).
    fn cut(&mut self) -> Option<usize> {
        if self.buf.len() < self.frame_bytes {
            return None;
        }
        if self.newline_free < self.frame_bytes {
            if let Some(i) = memrchr(b'\n', &self.buf[self.newline_free..self.frame_bytes]) {
                return Some(self.newline_free + i + 1);
            }
            self.newline_free = self.frame_bytes;
        }
        if let Some(i) = memchr(b'\n', &self.buf[self.newline_free..]) {
            return Some(self.newline_free + i + 1);
        }
        self.newline_free = self.buf.len();
        let limit = self.frame_bytes.saturating_mul(4);
        (self.buf.len() >= limit).then_some(limit)
    }
}

impl<W: Write> Write for FrameEncoder<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(n) = self.cut() {
            self.emit(n)?;
        }
        Ok(data.len())
    }

    /// Flushes the inner writer only; frames are cut by size, never by flush.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn read_exact_at<R: Read + Seek>(r: &mut R, at: u64, buf: &mut [u8]) -> io::Result<()> {
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(buf)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

/// Builds the frame table of a zstd file from its frame and block headers (no decoding,
/// except for frames whose header lacks the content size). Skippable frames are skipped.
pub fn scan_frames<R: Read + Seek>(r: &mut R) -> io::Result<Vec<ZstFrame>> {
    let len = r.seek(SeekFrom::End(0))?;
    let mut frames = Vec::new();
    let (mut c, mut u) = (0u64, 0u64);
    while c < len {
        let mut word = [0u8; 4];
        read_exact_at(r, c, &mut word)?;
        let magic = u32::from_le_bytes(word);
        if magic & SKIPPABLE_MASK == SKIPPABLE_MAGIC {
            read_exact_at(r, c + 4, &mut word)?;
            c += 8 + u64::from(u32::from_le_bytes(word));
            continue;
        }
        if word != ZSTD_MAGIC {
            return Err(invalid("not a zstd frame"));
        }
        let mut fhd = [0u8; 1];
        read_exact_at(r, c + 4, &mut fhd)?;
        let fhd = fhd[0];
        if fhd & 0x08 != 0 {
            return Err(invalid("zstd frame header reserved bit set"));
        }
        let single_segment = fhd & 0x20 != 0;
        let has_checksum = fhd & 0x04 != 0;
        let window_len = u64::from(!single_segment);
        let dict_len = [0u64, 1, 2, 4][usize::from(fhd & 0x03)];
        let fcs_len = match fhd >> 6 {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let fcs_at = c + 5 + window_len + dict_len;
        let content_size = if fcs_len == 0 {
            None
        } else {
            let mut fcs = [0u8; 8];
            read_exact_at(r, fcs_at, &mut fcs[..fcs_len])?;
            let v = u64::from_le_bytes(fcs);
            Some(if fcs_len == 2 { v + 256 } else { v })
        };
        let mut pos = fcs_at + fcs_len as u64;
        loop {
            let mut bh = [0u8; 4];
            read_exact_at(r, pos, &mut bh[..3])?;
            let bh = u32::from_le_bytes(bh);
            let block_size = u64::from(bh >> 3);
            let content = match (bh >> 1) & 0x03 {
                0 | 2 => block_size,
                1 => 1,
                _ => return Err(invalid("reserved zstd block type")),
            };
            pos += 3 + content;
            if bh & 1 != 0 {
                break;
            }
        }
        if has_checksum {
            pos += 4;
        }
        if pos > len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated zstd frame"));
        }
        let u_len = match content_size {
            Some(n) => n,
            None => {
                r.seek(SeekFrom::Start(c))?;
                let mut dec = zstd::stream::read::Decoder::new(r.by_ref().take(pos - c))?.single_frame();
                io::copy(&mut dec, &mut io::sink())?
            }
        };
        frames.push(ZstFrame {
            c_offset: c,
            c_len: pos - c,
            u_offset: u,
            u_len,
        });
        c = pos;
        u += u_len;
    }
    Ok(frames)
}

/// Decodes a whole zstd file (verifying frame checksums) and returns the decoded length.
pub fn verify_zst(path: &Path) -> Result<u64, CastError> {
    let mut dec = zstd::stream::read::Decoder::new(File::open(path)?)?;
    Ok(io::copy(&mut dec, &mut io::sink())?)
}

/// Options for [`compress_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressOptions {
    pub level: i32,
    pub frame_bytes: usize,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            level: DEFAULT_LEVEL,
            frame_bytes: DEFAULT_FRAME_BYTES,
        }
    }
}

/// Result of [`compress_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compressed {
    pub frames: Vec<ZstFrame>,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
}

/// Path of the temporary file [`compress_file`] writes next to `dst`.
pub fn tmp_path(dst: &Path) -> PathBuf {
    let mut name = dst.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Compresses `src` into `dst` atomically: writes `<dst>.tmp` (mode 0600), fsyncs it,
/// renames it to `dst` and fsyncs the directory. The source is left untouched; on any
/// error the temporary file is removed. Updating the DB and unlinking the source is the
/// caller's job.
pub fn compress_file(src: &Path, dst: &Path, opts: CompressOptions) -> Result<Compressed, CastError> {
    compress_from(File::open(src)?, dst, opts)
}

/// [`compress_file`] from an already opened source (e.g. one opened with `O_NOATIME`).
pub fn compress_from(input: impl Read, dst: &Path, opts: CompressOptions) -> Result<Compressed, CastError> {
    let tmp = tmp_path(dst);
    let result = write_compressed(input, &tmp, opts).and_then(|c| {
        fs::rename(&tmp, dst)?;
        if let Some(dir) = dst.parent().filter(|d| !d.as_os_str().is_empty()) {
            File::open(dir)?.sync_all()?;
        }
        Ok(c)
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_compressed(mut input: impl Read, tmp: &Path, opts: CompressOptions) -> Result<Compressed, CastError> {
    let out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    let mut enc = FrameEncoder::with_frame_bytes(BufWriter::with_capacity(1 << 20, out), opts.level, opts.frame_bytes)?;
    let uncompressed_bytes = io::copy(&mut input, &mut enc)?;
    let (out, frames) = enc.finish()?;
    let out = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    out.sync_all()?;
    let compressed_bytes = frames.iter().map(|f| f.c_len).sum();
    Ok(Compressed {
        frames,
        uncompressed_bytes,
        compressed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{ResumePoint, open, open_at};
    use crate::{CastReader, CastWriter, EventCode, Header, V3Writer};
    use std::io::Cursor;

    fn sample_cast(events: usize) -> Vec<u8> {
        let mut w = V3Writer::new(Vec::new());
        w.write_header(&Header::new_v3(80, 24, None, 1, None)).unwrap();
        for i in 0..events {
            w.write_event(
                i as u64 * 1000,
                EventCode::Output,
                &format!("line {i} {}\r\n", "x".repeat(i % 50)),
            )
            .unwrap();
        }
        w.into_inner()
    }

    fn encode(data: &[u8], frame_bytes: usize) -> (Vec<u8>, Vec<ZstFrame>) {
        let mut enc = FrameEncoder::with_frame_bytes(Vec::new(), 3, frame_bytes).unwrap();
        for chunk in data.chunks(777) {
            enc.write_all(chunk).unwrap();
        }
        enc.finish().unwrap()
    }

    #[test]
    fn magic() {
        assert!(is_zstd_magic(&ZSTD_MAGIC));
        assert!(is_zstd_magic(&[0x5A, 0x2A, 0x4D, 0x18, 0]));
        assert!(!is_zstd_magic(b"{\"ve"));
        assert!(!is_zstd_magic(&ZSTD_MAGIC[..3]));
    }

    #[test]
    fn frames_cut_at_newlines_and_scan_matches() {
        let data = sample_cast(3000);
        let (z, frames) = encode(&data, 16 * 1024);
        assert!(frames.len() > 5);
        for f in &frames {
            let end = (f.u_offset + f.u_len) as usize;
            assert_eq!(data[end - 1], b'\n');
        }
        assert_eq!(frames.iter().map(|f| f.u_len).sum::<u64>(), data.len() as u64);
        assert_eq!(scan_frames(&mut Cursor::new(&z)).unwrap(), frames);
        assert_eq!(zstd::decode_all(&z[..]).unwrap(), data);
    }

    #[test]
    fn long_line_cut() {
        let mut data = vec![b'a'; 100];
        data.push(b'\n');
        data.extend(vec![b'b'; 5000]);
        let (_, frames) = encode(&data, 1000);
        // No newline within the first frame size: the frame extends to the newline, then
        // the newline-free remainder is cut once it reaches four frames.
        assert_eq!(frames.iter().map(|f| f.u_len).collect::<Vec<_>>(), [101, 4000, 1000]);
    }

    /// The incremental newline scan cuts exactly where rescanning the whole buffer on
    /// every write does.
    #[test]
    fn incremental_scan_matches_full_rescan() {
        fn full_rescan_cut(buf: &[u8], fb: usize) -> Option<usize> {
            if buf.len() < fb {
                return None;
            }
            if let Some(i) = memrchr(b'\n', &buf[..fb]) {
                return Some(i + 1);
            }
            if let Some(i) = memchr(b'\n', &buf[fb..]) {
                return Some(fb + i + 1);
            }
            (buf.len() >= 4 * fb).then_some(4 * fb)
        }
        let mut data = Vec::new();
        for (run, nl) in [
            (50, true),
            (2500, true),
            (999, true),
            (7000, false),
            (3, true),
            (12_345, false),
        ] {
            data.extend(std::iter::repeat_n(b'x', run));
            if nl {
                data.push(b'\n');
            }
        }
        for step in [1, 7, 333, 1000, 4096, data.len()] {
            let mut expected = Vec::new();
            let mut buf = Vec::new();
            for chunk in data.chunks(step) {
                buf.extend_from_slice(chunk);
                while let Some(n) = full_rescan_cut(&buf, 1000) {
                    expected.push(n as u64);
                    buf.drain(..n);
                }
            }
            expected.push(buf.len() as u64);

            let mut enc = FrameEncoder::with_frame_bytes(Vec::new(), 1, 1000).unwrap();
            for chunk in data.chunks(step) {
                enc.write_all(chunk).unwrap();
            }
            let (z, frames) = enc.finish().unwrap();
            assert_eq!(
                frames.iter().map(|f| f.u_len).collect::<Vec<_>>(),
                expected,
                "step {step}"
            );
            assert_eq!(zstd::decode_all(&z[..]).unwrap(), data);
        }
    }

    #[test]
    fn scan_streaming_frames_without_content_size_and_skippable() {
        let mut z = Vec::new();
        z.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        z.extend_from_slice(&3u32.to_le_bytes());
        z.extend_from_slice(b"abc");
        let parts: [&[u8]; 2] = [b"hello\n", b"world, a bit longer\n"];
        for p in parts {
            let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
            enc.write_all(p).unwrap();
            z.extend(enc.finish().unwrap());
        }
        let frames = scan_frames(&mut Cursor::new(&z)).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].u_offset, frames[0].u_len), (0, 6));
        assert_eq!((frames[1].u_offset, frames[1].u_len), (6, 20));
        assert_eq!(frames[0].c_offset, 11);
        assert_eq!(frames[1].c_offset + frames[1].c_len, z.len() as u64);

        let truncated = &z[..z.len() - 2];
        assert!(scan_frames(&mut Cursor::new(truncated)).is_err());
    }

    #[test]
    fn compress_open_and_seek() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s.cast");
        let data = sample_cast(5000);
        fs::write(&src, &data).unwrap();
        // Extension deliberately wrong: detection is by magic.
        let dst = dir.path().join("s.cast.bin");
        let c = compress_file(
            &src,
            &dst,
            CompressOptions {
                level: 3,
                frame_bytes: 32 * 1024,
            },
        )
        .unwrap();
        assert!(c.frames.len() > 3);
        assert_eq!(c.uncompressed_bytes, data.len() as u64);
        assert_eq!(c.compressed_bytes, fs::metadata(&dst).unwrap().len());
        assert!(!tmp_path(&dst).exists());
        assert!(src.exists());
        assert_eq!(verify_zst(&dst).unwrap(), data.len() as u64);

        let plain: Vec<_> = {
            let mut r = open(&src).unwrap();
            std::iter::from_fn(|| r.next_event()).map(Result::unwrap).collect()
        };
        let mut r = open(&dst).unwrap();
        let mut checkpoints = Vec::new();
        let mut i = 0;
        while let Some(ev) = r.next_event() {
            assert_eq!(ev.unwrap(), plain[i]);
            i += 1;
            if i % 997 == 0 {
                checkpoints.push((
                    i,
                    ResumePoint {
                        offset: r.position(),
                        t_us: plain[i - 1].t_us,
                    },
                ));
            }
        }
        assert_eq!(i, plain.len());
        for (idx, ck) in checkpoints {
            for frames in [None, Some(&c.frames[..])] {
                let mut r = open_at(&dst, ck, frames).unwrap();
                let rest: Vec<_> = std::iter::from_fn(|| r.next_event()).map(Result::unwrap).collect();
                assert_eq!(rest, plain[idx..]);
            }
        }
        let beyond = ResumePoint {
            offset: data.len() as u64 + 5,
            t_us: 0,
        };
        assert!(matches!(open_at(&dst, beyond, None), Err(CastError::Format(_))));
        let mid_line = ResumePoint {
            offset: checkpoints_first_offset(&data) + 3,
            t_us: 0,
        };
        assert!(matches!(open_at(&dst, mid_line, None), Err(CastError::Format(_))));
    }

    fn checkpoints_first_offset(data: &[u8]) -> u64 {
        memchr(b'\n', data).unwrap() as u64 + 1
    }

    #[test]
    fn compress_failure_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("out.cast.zst");
        assert!(compress_file(&dir.path().join("missing.cast"), &dst, CompressOptions::default()).is_err());
        assert!(!tmp_path(&dst).exists());
        assert!(!dst.exists());
    }
}
