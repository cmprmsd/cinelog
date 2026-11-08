//! Read-only access to a legacy cast and its commands.log (§11 steps 1–2): every open is
//! `O_RDONLY|O_NOATIME`, nothing is ever written next to the files.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use cinelog_cast::{CastReader, Format, Header, Reader, ResumePoint, read_header};
use sha2::{Digest, Sha256};

use crate::classify::open_noatime;
use crate::error::{IndexError, Result};
use crate::model::FileStamp;

/// Bytes hashed for the legacy identity (`legacy_import.sha256`, [`crate::model::legacy_uuid`]).
pub(crate) const HASH_BYTES: u64 = 1 << 20;
const READ_BUFFER: usize = 256 * 1024;
/// Largest commands.log read (the biggest real one is a few hundred KiB).
const MAX_LOG_BYTES: u64 = 64 << 20;

/// A legacy cast opened for reading.
pub(crate) struct Source {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub uid: u32,
    pub sha256: String,
    pub header: Header,
    /// The cast is zstd-compressed; offsets are then positions in the decompressed stream.
    pub zstd: bool,
}

impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = open_noatime(path)?;
        let md = file.metadata()?;
        if !md.is_file() {
            return Err(IndexError::Invalid(format!("{}: not a regular file", path.display())));
        }
        let mut prefix = vec![0u8; HASH_BYTES.min(md.len()) as usize];
        let n = read_full_at(&file, &mut prefix)?;
        prefix.truncate(n);
        let sha256 = data_encoding::HEXLOWER.encode(&Sha256::digest(&prefix));
        let zstd = cinelog_cast::is_zstd_magic(&prefix[..prefix.len().min(4)]);
        let mut src = Self {
            path: path.to_owned(),
            stamp: FileStamp::from_metadata(&md),
            uid: md.uid(),
            sha256,
            header: Header::new_v3(80, 24, None, 0, None),
            zstd,
        };
        let (reader, _) = src.reader_from(file, None)?;
        src.header = reader.header().clone();
        Ok(src)
    }

    /// A reader positioned at the first event, or at `at` (a line boundary from an earlier
    /// read of the same file). Returns the reader and the offset of the first event.
    pub(crate) fn reader(&self, at: Option<ResumePoint>) -> Result<(Box<dyn CastReader + Send>, u64)> {
        let file = open_noatime(&self.path)?;
        self.reader_from(file, at)
    }

    fn reader_from(&self, file: File, at: Option<ResumePoint>) -> Result<(Box<dyn CastReader + Send>, u64)> {
        if self.zstd {
            let dec = zstd::stream::read::Decoder::new(file)?;
            let mut src = BufReader::with_capacity(READ_BUFFER, dec);
            let (header, format, first) = read_header(&mut src)?;
            let mut reader = Reader::resume(src, header, format, ResumePoint { offset: first, t_us: 0 });
            if let Some(at) = at.filter(|a| a.offset > first) {
                // No seek table for legacy casts: decode up to the checkpoint.
                while reader.resume_point().offset < at.offset {
                    match reader.next_event() {
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                    }
                }
                if reader.resume_point() != at {
                    return Err(IndexError::Invalid("checkpoint is not an event boundary".into()));
                }
            }
            return Ok((Box::new(reader), first));
        }
        let mut src = BufReader::with_capacity(READ_BUFFER, file);
        let (header, format, first) = read_header(&mut src)?;
        let at = match at.filter(|a| a.offset > first) {
            None => ResumePoint { offset: first, t_us: 0 },
            Some(at) => {
                let file = src.get_ref();
                let mut last = [0u8; 1];
                if at.offset > file.metadata()?.len()
                    || file.read_at(&mut last, at.offset - 1)? != 1
                    || !matches!(last[0], b'\n' | b']')
                {
                    return Err(IndexError::Invalid("checkpoint is not an event boundary".into()));
                }
                src.seek(SeekFrom::Start(at.offset))?;
                at
            }
        };
        Ok((boxed(src, header, format, at), first))
    }
}

fn boxed<R: std::io::BufRead + Send + 'static>(
    src: R,
    header: Header,
    format: Format,
    at: ResumePoint,
) -> Box<dyn CastReader + Send> {
    Box::new(Reader::resume(src, header, format, at))
}

fn read_full_at(file: &File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// `<cast>.commands.log` next to `cast`; for a compressed `<cast>.zst` the log of the plain
/// name (the old plugin wrote it before the cast was compressed).
pub fn log_path(cast: &Path) -> PathBuf {
    let plain = match cast.extension() {
        Some(e) if e == "zst" => cast.with_extension(""),
        _ => cast.to_owned(),
    };
    let mut p = plain.into_os_string();
    p.push(".commands.log");
    PathBuf::from(p)
}

/// The commands.log text (lossy UTF-8) and its stamp, `None` when there is none.
pub(crate) fn read_log(path: &Path) -> Result<Option<(String, FileStamp)>> {
    let file = match open_noatime(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let md = file.metadata()?;
    if !md.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(md.len().min(MAX_LOG_BYTES) as usize);
    file.take(MAX_LOG_BYTES).read_to_end(&mut bytes)?;
    // NUL holes (a crash while appending) are dropped; they only split lines.
    bytes.retain(|&b| b != 0);
    Ok(Some((
        String::from_utf8_lossy(&bytes).into_owned(),
        FileStamp::from_metadata(&md),
    )))
}

/// Session user of a legacy cast: TEAM casts live in `<logs_root>/<user>/…`; PERSONAL ones
/// belong to the file owner.
pub(crate) fn team_user(logs_root: &Path, cast: &Path) -> Option<String> {
    let rel = cast.strip_prefix(logs_root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?;
    // The cast itself directly in the root has no user directory.
    comps.next()?;
    first.as_os_str().to_str().map(str::to_owned)
}

pub(crate) fn user_name(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map_or_else(|| uid.to_string(), |u| u.name)
}

/// `started_at` when the header has no timestamp: the old plugin's file name
/// `terminal_%F_%T:%3N.cast` in the host's zone, else the file's mtime.
pub(crate) fn start_from_name(path: &Path) -> Option<f64> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("terminal_")?;
    let stamp = rest.get(..23)?; // 2024-01-02_03:04:05:678
    let (date, time) = stamp.split_once('_')?;
    let (hms, ms) = time.rsplit_once(':')?;
    let dt: jiff::civil::DateTime = format!("{date}T{hms}.{ms}").parse().ok()?;
    let z = dt.to_zoned(jiff::tz::TimeZone::system()).ok()?;
    Some(z.timestamp().as_millisecond() as f64 / 1e3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const HDR: &str = r#"{"version":3,"term":{"cols":100,"rows":30,"type":"xterm"},"timestamp":1700000000}"#;

    #[test]
    fn opens_plain_and_zstd_and_resumes() {
        let d = tempfile::tempdir().unwrap();
        let body = format!("{HDR}\n[0.5, \"o\", \"a\"]\n[0.5, \"o\", \"b\"]\n");
        let plain = d.path().join("p.cast");
        std::fs::write(&plain, &body).unwrap();
        let zst = d.path().join("z.cast");
        let mut enc = zstd::stream::write::Encoder::new(File::create(&zst).unwrap(), 3).unwrap();
        enc.write_all(body.as_bytes()).unwrap();
        enc.finish().unwrap();
        for p in [&plain, &zst] {
            let s = Source::open(p).unwrap();
            assert_eq!((s.header.term.cols, s.header.timestamp), (100, Some(1_700_000_000)));
            let (mut r, first) = s.reader(None).unwrap();
            assert_eq!(first, HDR.len() as u64 + 1);
            assert_eq!(r.next_event().unwrap().unwrap().data, "a");
            let at = ResumePoint {
                offset: r.position(),
                t_us: 500_000,
            };
            let (mut r2, _) = s.reader(Some(at)).unwrap();
            let e = r2.next_event().unwrap().unwrap();
            assert_eq!((e.data.as_str(), e.t_us), ("b", 1_000_000));
            let bad = ResumePoint {
                offset: at.offset + 2,
                t_us: 0,
            };
            assert!(s.reader(Some(bad)).is_err());
        }
        let raw = std::fs::read(&plain).unwrap();
        let s = Source::open(&plain).unwrap();
        assert_eq!(s.sha256, data_encoding::HEXLOWER.encode(&Sha256::digest(&raw)));
        assert_eq!(s.stamp.size, raw.len() as u64);
    }

    #[test]
    fn logs_and_users() {
        let d = tempfile::tempdir().unwrap();
        let cast = d.path().join("terminal_2024-01-02_03:04:05:678.cast");
        assert_eq!(
            log_path(&cast).file_name().unwrap(),
            "terminal_2024-01-02_03:04:05:678.cast.commands.log"
        );
        assert_eq!(
            log_path(&d.path().join("a.cast.zst")),
            d.path().join("a.cast.commands.log")
        );
        assert!(read_log(&log_path(&cast)).unwrap().is_none());
        std::fs::write(log_path(&cast), b"a\0b\n").unwrap();
        assert_eq!(read_log(&log_path(&cast)).unwrap().unwrap().0, "ab\n");
        let z = jiff::civil::date(2024, 1, 2)
            .at(3, 4, 5, 678_000_000)
            .to_zoned(jiff::tz::TimeZone::system())
            .unwrap();
        assert_eq!(
            start_from_name(&cast),
            Some(z.timestamp().as_millisecond() as f64 / 1e3)
        );
        assert_eq!(start_from_name(Path::new("other.cast")), None);
        let root = Path::new("/root/logs");
        assert_eq!(
            team_user(root, Path::new("/root/logs/alice/x/a.cast")).as_deref(),
            Some("alice")
        );
        assert_eq!(team_user(root, Path::new("/root/logs/a.cast")), None);
        assert_eq!(team_user(root, Path::new("/elsewhere/a.cast")), None);
        assert_eq!(user_name(0), "root");
    }
}
