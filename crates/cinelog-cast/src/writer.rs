//! asciicast v3 writer.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::{CastError, CastWriter, EventCode, Header};

/// Buffer size of the file writer returned by [`create`].
pub const WRITE_BUFFER: usize = 64 * 1024;

/// How [`CastWriter::sync`] pushes flushed data to stable storage.
pub type SyncFn<W> = fn(&mut W) -> io::Result<()>;

/// Append-only asciicast v3 writer (see [`CastWriter`] for the contract).
///
/// Lines look like `[0.051234, "o", "text"]`: the interval since the previous event with
/// exactly six decimals, so summing on read is exact in µs.
///
/// Any [`Write`] sink works; [`CastWriter::sync`] flushes it and then runs the sync hook
/// (none for [`V3Writer::new`], `fdatasync` for [`create`], custom via
/// [`V3Writer::with_sync`]).
#[derive(Debug)]
pub struct V3Writer<W: Write> {
    w: W,
    sync_fn: SyncFn<W>,
    line: Vec<u8>,
    last_us: u64,
    bytes: u64,
    header_written: bool,
    /// A failed write may have left part of a line in the sink.
    broken: bool,
}

impl<W: Write> V3Writer<W> {
    /// A writer whose `sync` only flushes (in-memory sinks, pipes, test fakes).
    pub fn new(w: W) -> Self {
        Self::with_sync(w, |_| Ok(()))
    }

    /// A writer whose `sync` flushes and then calls `sync_fn` on the sink.
    pub fn with_sync(w: W, sync_fn: SyncFn<W>) -> Self {
        Self {
            w,
            sync_fn,
            line: Vec::with_capacity(4096),
            last_us: 0,
            bytes: 0,
            header_written: false,
            broken: false,
        }
    }

    /// Absolute offset of the last event written (after clamping).
    pub fn last_t_us(&self) -> u64 {
        self.last_us
    }

    pub fn get_ref(&self) -> &W {
        &self.w
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }

    pub fn into_inner(self) -> W {
        self.w
    }

    /// Writes the pending line. After a failed write (ENOSPC, EIO) the sink may end in a
    /// fragment, so the next line starts with a newline: the fragment stays one bad line
    /// instead of swallowing the next event (e.g. the recorder's `error` marker).
    fn emit_line(&mut self) -> Result<(), CastError> {
        if self.broken {
            self.w.write_all(b"\n")?;
            self.bytes += 1;
            self.broken = false;
        }
        if let Err(e) = self.w.write_all(&self.line) {
            self.broken = true;
            return Err(e.into());
        }
        self.bytes += self.line.len() as u64;
        Ok(())
    }
}

impl<W: Write> CastWriter for V3Writer<W> {
    fn write_header(&mut self, header: &Header) -> Result<(), CastError> {
        if self.header_written {
            return Err(CastError::Format("header written twice".into()));
        }
        self.line.clear();
        serde_json::to_writer(&mut self.line, header).map_err(|e| CastError::Header(e.to_string()))?;
        self.line.push(b'\n');
        self.emit_line()?;
        self.header_written = true;
        Ok(())
    }

    fn write_event(&mut self, t_us: u64, code: EventCode, data: &str) -> Result<(), CastError> {
        if !self.header_written {
            return Err(CastError::Format("event before header".into()));
        }
        let t_us = t_us.max(self.last_us);
        let interval = t_us - self.last_us;
        self.line.clear();
        write!(self.line, "[{}.{:06}, ", interval / 1_000_000, interval % 1_000_000)?;
        match code {
            EventCode::Other(c) => serde_json::to_writer(&mut self.line, &c.to_string()).map_err(io::Error::from)?,
            known => write!(self.line, "\"{}\"", known.as_char())?,
        }
        self.line.extend_from_slice(b", ");
        serde_json::to_writer(&mut self.line, data).map_err(io::Error::from)?;
        self.line.extend_from_slice(b"]\n");
        self.emit_line()?;
        self.last_us = t_us;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CastError> {
        Ok(self.w.flush()?)
    }

    fn sync(&mut self) -> Result<(), CastError> {
        self.w.flush()?;
        Ok((self.sync_fn)(&mut self.w)?)
    }

    fn bytes_written(&self) -> u64 {
        self.bytes
    }
}

/// Creates a new cast file (`O_EXCL`, mode 0600; fails if it exists) with a buffered writer.
pub fn create(path: &Path) -> Result<V3Writer<BufWriter<File>>, CastError> {
    let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    Ok(V3Writer::with_sync(
        BufWriter::with_capacity(WRITE_BUFFER, file),
        sync_buffered_file,
    ))
}

fn sync_buffered_file(w: &mut BufWriter<File>) -> io::Result<()> {
    w.flush()?;
    w.get_ref().sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn header() -> Header {
        Header::new_v3(80, 24, Some("xterm".into()), 1_700_000_000, Some("/bin/zsh".into()))
    }

    fn written(events: &[(u64, EventCode, &str)]) -> String {
        let mut w = V3Writer::new(Vec::new());
        w.write_header(&header()).unwrap();
        for &(t, c, d) in events {
            w.write_event(t, c, d).unwrap();
        }
        assert_eq!(w.bytes_written(), w.get_ref().len() as u64);
        String::from_utf8(w.into_inner()).unwrap()
    }

    #[test]
    fn writes_intervals_with_six_decimals() {
        let out = written(&[
            (0, EventCode::Output, "a"),
            (1_500_000, EventCode::Input, "b"),
            (1_500_001, EventCode::Marker, "{}"),
            (900_000, EventCode::Resize, "100x30"),
            (62_000_000, EventCode::Exit, "0"),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with(r#"{"version":3,"term":{"cols":80"#));
        assert_eq!(
            &lines[1..],
            [
                r#"[0.000000, "o", "a"]"#,
                r#"[1.500000, "i", "b"]"#,
                r#"[0.000001, "m", "{}"]"#,
                r#"[0.000000, "r", "100x30"]"#,
                r#"[60.499999, "x", "0"]"#,
            ]
        );
    }

    #[test]
    fn escapes_data_and_odd_codes() {
        let out = written(&[
            (0, EventCode::Output, "\u{1b}[1m\"q\"\\\n\u{0}ü"),
            (0, EventCode::Other('"'), ""),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], r#"[0.000000, "o", "\u001b[1m\"q\"\\\n\u0000ü"]"#);
        assert_eq!(lines[2], r#"[0.000000, "\"", ""]"#);
        let raw = crate::parse_event_line(lines[2].as_bytes()).unwrap();
        assert_eq!(raw.code, EventCode::Other('"'));
    }

    #[test]
    fn ordering_errors() {
        let mut w = V3Writer::new(Vec::new());
        assert!(matches!(
            w.write_event(0, EventCode::Output, "x"),
            Err(CastError::Format(_))
        ));
        w.write_header(&header()).unwrap();
        assert!(matches!(w.write_header(&header()), Err(CastError::Format(_))));
    }

    #[test]
    fn create_is_exclusive_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.cast");
        let mut w = create(&path).unwrap();
        w.write_header(&header()).unwrap();
        w.write_event(10, EventCode::Output, "x").unwrap();
        w.sync().unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), w.bytes_written());
        assert!(matches!(create(&path), Err(CastError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists));
    }

    #[test]
    fn line_after_failed_write_starts_fresh() {
        /// Accepts `budget` bytes, then fails once with ENOSPC (after a short write).
        struct Full {
            out: Vec<u8>,
            budget: usize,
            failed: bool,
        }
        impl Write for Full {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.budget == 0 && !self.failed {
                    self.failed = true;
                    return Err(io::Error::from_raw_os_error(28));
                }
                let n = buf.len().min(if self.failed { usize::MAX } else { self.budget });
                self.budget -= n.min(self.budget);
                self.out.extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        // Escaped output beyond the buffer capacity bypasses the buffer, as in `create`.
        let sink = Full {
            out: Vec::new(),
            budget: usize::MAX,
            failed: false,
        };
        let mut w = V3Writer::new(BufWriter::with_capacity(WRITE_BUFFER, sink));
        w.write_header(&header()).unwrap();
        w.write_event(1_000_000, EventCode::Output, "a").unwrap();
        w.flush().unwrap();
        w.get_mut().get_mut().budget = 7;
        let big = "\u{1b}".repeat(20_000);
        assert!(matches!(
            w.write_event(2_000_000, EventCode::Output, &big),
            Err(CastError::Io(_))
        ));
        w.write_event(3_000_000, EventCode::Marker, r#"{"k":"error"}"#).unwrap();
        w.flush().unwrap();
        let out = &w.get_ref().get_ref().out;
        assert_eq!(w.bytes_written(), out.len() as u64 - 7);

        let mut r = crate::Reader::new(out.as_slice()).unwrap();
        let evs: Vec<_> = std::iter::from_fn(|| crate::CastReader::next_event(&mut r))
            .map(|e| e.unwrap())
            .map(|e| (e.t_us, e.code, e.data))
            .collect();
        assert_eq!(
            evs,
            [
                (1_000_000, EventCode::Output, "a".to_owned()),
                (3_000_000, EventCode::Marker, r#"{"k":"error"}"#.to_owned()),
            ]
        );
        assert_eq!(crate::CastReader::stats(&r).bad_lines, 1);
    }

    /// Any `Write` sink is a `CastWriter` (contract): a sink that is not a file, and a
    /// custom sync hook.
    #[test]
    fn plain_write_sinks_and_sync_hook() {
        struct Slow(Vec<u8>, u32);
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let n = buf.len().min(3);
                self.0.extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        fn assert_cast_writer<T: CastWriter>(_: &T) {}

        let mut w = V3Writer::new(Slow(Vec::new(), 0));
        assert_cast_writer(&w);
        w.write_header(&header()).unwrap();
        w.write_event(5, EventCode::Output, "hello").unwrap();
        w.sync().unwrap();
        assert_eq!(w.get_ref().1, 0);
        assert_eq!(w.bytes_written(), w.get_ref().0.len() as u64);

        let mut w = V3Writer::with_sync(Slow(Vec::new(), 0), |s| {
            s.1 += 1;
            Ok(())
        });
        w.write_header(&header()).unwrap();
        w.sync().unwrap();
        w.sync().unwrap();
        assert_eq!(w.get_ref().1, 2);

        let mut w = V3Writer::with_sync(io::sink(), |_| Err(io::Error::other("disk gone")));
        w.write_header(&header()).unwrap();
        assert!(matches!(w.sync(), Err(CastError::Io(_))));
    }
}
