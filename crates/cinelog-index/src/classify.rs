//! Telling cinelog casts from legacy casts (plan §11 step 1), without ever writing to or
//! updating the atime of the file.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use cinelog_cast::{CastReader, EventCode, Reader};
use cinelog_proto::CastMarker;

use crate::error::Result;

/// What a `.cast` / `.cast.zst` file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastKind {
    /// Starts with a cinelog `session` marker at t=0. Parts ≥ 2 belong to the session of
    /// part 1 and are indexed through it.
    Cinelog { uuid: String, part: u32 },
    /// A readable cast without a `session` marker (old asciinema-based plugin).
    Legacy,
    /// Empty, a header that does not parse, or no cast at all.
    NotACast,
}

/// Opens `path` read-only with `O_NOATIME` (dropped when the kernel refuses it because we
/// do not own the file).
pub fn open_noatime(path: &Path) -> io::Result<File> {
    match OpenOptions::new().read(true).custom_flags(libc::O_NOATIME).open(path) {
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => File::open(path),
        r => r,
    }
}

/// Reads the header and the first event.
pub fn classify(path: &Path) -> Result<CastKind> {
    let file = open_noatime(path)?;
    let mut magic = [0u8; 4];
    let n = read_prefix(&file, &mut magic)?;
    let reader: Box<dyn CastReader> = if cinelog_cast::is_zstd_magic(&magic[..n]) {
        let dec = zstd::stream::read::Decoder::new(file)?;
        match Reader::new(BufReader::new(dec)) {
            Ok(r) => Box::new(r),
            Err(_) => return Ok(CastKind::NotACast),
        }
    } else {
        match Reader::new(BufReader::new(file)) {
            Ok(r) => Box::new(r),
            Err(_) => return Ok(CastKind::NotACast),
        }
    };
    Ok(classify_reader(reader)?)
}

fn classify_reader(mut reader: Box<dyn CastReader>) -> std::result::Result<CastKind, cinelog_cast::CastError> {
    match reader.next_event().transpose()? {
        Some(ev) if ev.t_us == 0 && ev.code == EventCode::Marker => match CastMarker::from_json(&ev.data) {
            Ok(Some(CastMarker::Session(info))) => Ok(CastKind::Cinelog {
                uuid: info.uuid,
                part: info.part.max(1),
            }),
            _ => Ok(CastKind::Legacy),
        },
        _ => Ok(CastKind::Legacy),
    }
}

fn read_prefix(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    const HDR: &str = r#"{"version":3,"term":{"cols":80,"rows":24},"timestamp":1731070000}"#;

    #[test]
    fn kinds() {
        let d = tempfile::tempdir().unwrap();
        let session = r#"{\"cl\":1,\"k\":\"session\",\"uuid\":\"01934f5e-7b2a-7c3d-8e4f-0123456789ab\",\"part\":2,\"start_epoch_us\":1,\"scope\":\"personal\",\"user\":\"u\",\"attribution\":\"ancestor\"}"#;
        let new = write(
            d.path(),
            "a.cast",
            &format!("{HDR}\n[0.000000, \"m\", \"{session}\"]\n"),
        );
        assert_eq!(
            classify(&new).unwrap(),
            CastKind::Cinelog {
                uuid: "01934f5e-7b2a-7c3d-8e4f-0123456789ab".into(),
                part: 2
            }
        );
        let legacy = write(d.path(), "b.cast", &format!("{HDR}\n[0.5, \"o\", \"$ \"]\n"));
        assert_eq!(classify(&legacy).unwrap(), CastKind::Legacy);
        let label = write(d.path(), "c.cast", &format!("{HDR}\n[0.0, \"m\", \"chapter\"]\n"));
        assert_eq!(classify(&label).unwrap(), CastKind::Legacy);
        let empty_events = write(d.path(), "d.cast", &format!("{HDR}\n"));
        assert_eq!(classify(&empty_events).unwrap(), CastKind::Legacy);
        let junk = write(d.path(), "e.cast", "not a cast\n");
        assert_eq!(classify(&junk).unwrap(), CastKind::NotACast);
        let empty = write(d.path(), "f.cast", "");
        assert_eq!(classify(&empty).unwrap(), CastKind::NotACast);
        assert!(classify(&d.path().join("missing.cast")).is_err());
    }

    #[test]
    fn zstd_casts_are_read_transparently() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("z.cast.zst");
        let mut enc = zstd::stream::write::Encoder::new(std::fs::File::create(&p).unwrap(), 3).unwrap();
        writeln!(enc, "{HDR}\n[0.5, \"o\", \"x\"]").unwrap();
        enc.finish().unwrap();
        assert_eq!(classify(&p).unwrap(), CastKind::Legacy);
    }
}
