//! Writer thread (plan §5.1, §5.8): turns events into asciicast v3 lines, flushes whenever
//! the queue runs empty, fsyncs periodically, rotates parts by size and age, and on an I/O
//! error closes the cast and enters degraded mode (passthrough only) until `cinelog resume`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cinelog_cast::{CastError, CastWriter, EventCode, Header, Utf8Accumulator, format_resize};
use cinelog_core::clock::{Clock, local_tz};
use cinelog_core::logging::RecLog;
use cinelog_proto::{CastMarker, ErrorInfo, Extra, GapInfo, Nonce, RotateInfo, RotateReason, SessionInfo, scrub_nonce};
use serde_json::{Map, Value};

use crate::events::{EventRx, Msg, Payload};
use crate::reg::RegistryHandle;

/// Creates the cast file of a part (`O_EXCL`, 0600).
pub(crate) trait CastFactory: Send {
    /// File name of `part` (all parts share the first part's stem).
    fn file_name(&self, part: u32) -> String;
    fn create(&mut self, part: u32) -> Result<(Box<dyn CastWriter + Send>, PathBuf), CastError>;
}

/// Parts are created next to the first one; part 1 is created before the fork (so a
/// failure can still fall back to an unrecorded shell) and handed in ready-made.
pub(crate) struct FileFactory {
    dir: PathBuf,
    name: Box<dyn Fn(u32) -> String + Send>,
    first: Option<(Box<dyn CastWriter + Send>, PathBuf)>,
}

impl FileFactory {
    pub(crate) fn new(
        dir: PathBuf,
        name: impl Fn(u32) -> String + Send + 'static,
        first: (Box<dyn CastWriter + Send>, PathBuf),
    ) -> Self {
        Self {
            dir,
            name: Box::new(name),
            first: Some(first),
        }
    }
}

impl CastFactory for FileFactory {
    fn file_name(&self, part: u32) -> String {
        (self.name)(part)
    }

    fn create(&mut self, part: u32) -> Result<(Box<dyn CastWriter + Send>, PathBuf), CastError> {
        if part == 1
            && let Some(first) = self.first.take()
        {
            return Ok(first);
        }
        let path = self.dir.join(self.file_name(part));
        let w = cinelog_cast::create(&path)?;
        Ok((Box::new(w), path))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriterConfig {
    /// `record.rotate_mb` in bytes; 0 disables size rotation.
    pub rotate_bytes: u64,
    /// `record.rotate_days` in µs; 0 disables age rotation.
    pub rotate_age_us: u64,
    /// `record.fsync_secs` in µs; 0 syncs only at rotation and exit.
    pub fsync_us: u64,
}

/// Header fields that stay the same for every part.
#[derive(Debug, Clone)]
pub(crate) struct HeaderInfo {
    pub cols: u16,
    pub rows: u16,
    pub term: Option<String>,
    pub shell: Option<String>,
}

/// State the writer shares with the I/O threads.
#[derive(Debug, Default)]
pub(crate) struct WriterShared {
    pub degraded: AtomicBool,
}

struct Part {
    w: Box<dyn CastWriter + Send>,
    path: PathBuf,
    no: u32,
    /// Boottime of t=0 of this part.
    t0: u64,
    dirty: bool,
    last_sync: u64,
}

pub(crate) struct Writer {
    factory: Box<dyn CastFactory>,
    clock: Arc<dyn Clock>,
    template: SessionInfo,
    header: HeaderInfo,
    cfg: WriterConfig,
    shared: Arc<WriterShared>,
    registry: Option<Arc<RegistryHandle>>,
    log: Option<RecLog>,
    /// Scrubbed from every data string (PROTOCOL §5.1: the nonce never reaches a cast).
    nonce: Option<Nonce>,
    /// Trailing hex run of the last `o` text, held back in case the next read completes a
    /// nonce. Written with its own timestamp before the next non-`o` event, or once it is
    /// [`HOLD_US`] old (idle flushes keep it, a read boundary is usually just one wakeup).
    held: Option<(u64, String)>,
    part: Option<Part>,
    /// Number of the last part opened (also while degraded).
    last_part: u32,
    last_name: Option<String>,
    out_acc: Utf8Accumulator,
    in_acc: Utf8Accumulator,
    lossy_bytes: u64,
    /// Earliest boottime at which a failed rotation is retried.
    rotate_retry_at: u64,
    /// The `error` marker of the I/O error that degraded the recording, when it could not be
    /// written to the failing part; it opens the continuation part instead.
    pending_error: Option<ErrorInfo>,
    exited: bool,
    scratch: String,
}

/// Counters reported at the end of a recording.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WriterStats {
    pub lossy_bytes: u64,
    pub parts: u32,
}

const IDLE_TICK: Duration = Duration::from_millis(500);
/// Longest time a held hex tail waits for the rest of a split nonce.
const HOLD_US: u64 = 1_000_000;
const ROTATE_RETRY_US: u64 = 60_000_000;

fn error_code(e: &CastError) -> &'static str {
    match e {
        CastError::Io(io) => match io.raw_os_error() {
            Some(libc::ENOSPC) => "enospc",
            Some(libc::EIO) => "eio",
            Some(libc::EDQUOT) => "edquot",
            _ => "write",
        },
        _ => "write",
    }
}

fn error_info(e: &CastError) -> ErrorInfo {
    ErrorInfo {
        error: error_code(e).into(),
        detail: Some(e.to_string()),
        degraded: true,
        extra: Extra::new(),
    }
}

impl Writer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        factory: Box<dyn CastFactory>,
        clock: Arc<dyn Clock>,
        template: SessionInfo,
        header: HeaderInfo,
        cfg: WriterConfig,
        shared: Arc<WriterShared>,
        registry: Option<Arc<RegistryHandle>>,
        log: Option<RecLog>,
        nonce: Option<Nonce>,
    ) -> Self {
        Self {
            factory,
            clock,
            template,
            header,
            cfg,
            shared,
            registry,
            log,
            nonce,
            held: None,
            part: None,
            last_part: 0,
            last_name: None,
            out_acc: Utf8Accumulator::new(),
            in_acc: Utf8Accumulator::new(),
            lossy_bytes: 0,
            rotate_retry_at: 0,
            pending_error: None,
            exited: false,
            scratch: String::new(),
        }
    }

    /// Opens part 1 at `t0` (`template` carries its start times) and processes events
    /// until every sender is gone.
    pub(crate) fn run(mut self, rx: EventRx, t0: u64) -> WriterStats {
        let first = self.template.clone();
        self.open_part(1, t0, first);
        loop {
            let msg = match rx.try_recv() {
                Ok(m) => m,
                Err(true) => break,
                Err(false) => {
                    self.release_stale_held();
                    self.flush();
                    match rx.recv_timeout(self.idle_timeout()) {
                        Ok(m) => m,
                        Err(true) => break,
                        Err(false) => {
                            if self.release_stale_held() {
                                self.flush();
                            }
                            self.housekeeping(self.clock.boottime_us());
                            continue;
                        }
                    }
                }
            };
            let now = match &msg {
                Msg::Event { t, .. } | Msg::ResumeDegraded { t } => *t,
                Msg::Gap { to, .. } => *to,
            };
            self.handle(msg);
            self.housekeeping(now);
        }
        self.close(true);
        WriterStats {
            lossy_bytes: self.lossy_bytes,
            parts: self.last_part,
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Event { t, payload } => self.event(t, payload),
            Msg::Gap {
                dropped_bytes,
                from,
                to,
            } => {
                let Some(t0) = self.part.as_ref().map(|p| p.t0) else {
                    return;
                };
                let secs = |b: u64| b.saturating_sub(t0) as f64 / 1e6;
                self.marker(
                    to,
                    &CastMarker::Gap(GapInfo {
                        dropped_bytes,
                        from_t: secs(from),
                        to_t: secs(to),
                        extra: Extra::new(),
                    }),
                );
            }
            Msg::ResumeDegraded { t } => {
                if self.part.is_none() && !self.exited {
                    let mut info = self.template_at(t);
                    info.continues = self.last_name.clone();
                    self.open_part(self.last_part + 1, t, info);
                }
            }
        }
    }

    fn event(&mut self, t: u64, payload: Payload) {
        if self.exited {
            return;
        }
        match payload {
            Payload::Output(bytes) => {
                let mut s = std::mem::take(&mut self.scratch);
                s.clear();
                self.lossy_bytes += self.out_acc.push(&bytes, &mut s);
                self.output(t, &s);
                self.scratch = s;
            }
            Payload::Input(bytes) => {
                let mut s = String::new();
                self.lossy_bytes += self.in_acc.push(&bytes, &mut s);
                if !s.is_empty() {
                    self.write(t, EventCode::Input, &s);
                }
            }
            Payload::Marker(m) => self.marker(t, &m),
            Payload::Resize { cols, rows } => {
                self.header.cols = cols;
                self.header.rows = rows;
                self.write(t, EventCode::Resize, &format_resize(cols, rows));
            }
            Payload::Exit {
                status,
                bad_markers,
                dropped_bytes,
            } => {
                self.flush_utf8_tails(t);
                self.marker(t, &self.stats_marker(bad_markers, dropped_bytes));
                self.write(t, EventCode::Exit, &status.to_string());
                self.exited = true;
            }
        }
    }

    /// `stats` (an additive kind, PROTOCOL.md §7): per-session counters for the indexer.
    fn stats_marker(&self, bad_markers: u64, dropped_bytes: u64) -> CastMarker {
        let mut fields = Map::new();
        fields.insert("bad_markers".into(), Value::from(bad_markers));
        fields.insert("dropped_bytes".into(), Value::from(dropped_bytes));
        fields.insert("lossy_bytes".into(), Value::from(self.lossy_bytes));
        fields.insert("parts".into(), Value::from(self.last_part));
        CastMarker::Unknown {
            k: "stats".into(),
            fields,
        }
    }

    fn marker(&mut self, t: u64, m: &CastMarker) {
        self.write(t, EventCode::Marker, &m.to_json());
    }

    /// Writes `o` text with the nonce scrubbed, also when a read boundary split it.
    fn output(&mut self, t: u64, s: &str) {
        let Some(nonce) = self.nonce else {
            if !s.is_empty() {
                self.write(t, EventCode::Output, s);
            }
            return;
        };
        let mut text = match self.held.take() {
            Some((_, mut h)) => {
                h.push_str(s);
                h
            }
            None => s.to_owned(),
        };
        if let std::borrow::Cow::Owned(scrubbed) = scrub_nonce(&text, &nonce) {
            text = scrubbed;
        }
        let run = text
            .bytes()
            .rev()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count()
            .min(Nonce::HEX_LEN - 1);
        if run > 0 {
            let tail = text.split_off(text.len() - run);
            self.held = Some((t, tail));
        }
        if !text.is_empty() {
            self.write(t, EventCode::Output, &text);
        }
    }

    fn release_held(&mut self) {
        if let Some((t, h)) = self.held.take() {
            self.write(t, EventCode::Output, &h);
        }
    }

    /// Releases the held tail once it is [`HOLD_US`] old; returns whether it did.
    fn release_stale_held(&mut self) -> bool {
        let now = self.clock.boottime_us();
        if self
            .held
            .as_ref()
            .is_some_and(|(t, _)| now.saturating_sub(*t) >= HOLD_US)
        {
            self.release_held();
            return true;
        }
        false
    }

    /// Wait for the next message: [`IDLE_TICK`], or less while a held tail is due.
    fn idle_timeout(&self) -> Duration {
        let Some((t, _)) = &self.held else { return IDLE_TICK };
        let age = self.clock.boottime_us().saturating_sub(*t);
        IDLE_TICK.min(Duration::from_micros(HOLD_US.saturating_sub(age)).max(Duration::from_millis(1)))
    }

    fn flush_utf8_tails(&mut self, t: u64) {
        let mut s = String::new();
        self.lossy_bytes += self.out_acc.finish(&mut s);
        self.output(t, &s);
        self.release_held();
        s.clear();
        self.lossy_bytes += self.in_acc.finish(&mut s);
        if !s.is_empty() {
            self.write(t, EventCode::Input, &s);
        }
    }

    fn write(&mut self, t: u64, code: EventCode, data: &str) {
        if code != EventCode::Output {
            self.release_held();
        }
        let scrubbed = match (&self.nonce, code) {
            (Some(n), EventCode::Input | EventCode::Marker) => scrub_nonce(data, n),
            _ => std::borrow::Cow::Borrowed(data),
        };
        let Some(part) = self.part.as_mut() else { return };
        let rel = t.saturating_sub(part.t0);
        match part.w.write_event(rel, code, &scrubbed) {
            Ok(()) => part.dirty = true,
            Err(e) => self.fail(t, &e),
        }
    }

    fn flush(&mut self) {
        let Some(part) = self.part.as_mut() else { return };
        if let Err(e) = part.w.flush() {
            let t = self.clock.boottime_us();
            self.fail(t, &e);
        }
    }

    /// Periodic fsync and rotation checks.
    fn housekeeping(&mut self, now: u64) {
        let Some(part) = self.part.as_mut() else { return };
        if self.cfg.fsync_us > 0 && part.dirty && now.saturating_sub(part.last_sync) >= self.cfg.fsync_us {
            match part.w.sync() {
                Ok(()) => {
                    part.dirty = false;
                    part.last_sync = now;
                }
                Err(e) => return self.fail(now, &e),
            }
        }
        if self.exited || now < self.rotate_retry_at {
            return;
        }
        let reason = if self.cfg.rotate_bytes > 0 && part.w.bytes_written() >= self.cfg.rotate_bytes {
            RotateReason::Size
        } else if self.cfg.rotate_age_us > 0 && now.saturating_sub(part.t0) >= self.cfg.rotate_age_us {
            RotateReason::Age
        } else {
            return;
        };
        self.rotate(now, reason);
    }

    /// SessionInfo for a part starting at boottime `t`.
    fn template_at(&self, t: u64) -> SessionInfo {
        let mut info = self.template.clone();
        let now_boot = self.clock.boottime_us();
        let epoch = self.clock.realtime_us() - i64::try_from(now_boot.saturating_sub(t)).unwrap_or(0);
        let (tz, offset) = local_tz(epoch);
        info.start_epoch_us = epoch;
        info.boottime_us = t;
        info.tz = tz;
        info.tz_offset_s = offset;
        info
    }

    fn rotate(&mut self, t: u64, reason: RotateReason) {
        let Some(old) = self.part.as_ref() else { return };
        let next_no = old.no + 1;
        let old_name = file_name_of(&old.path);
        let new = match self.factory.create(next_no) {
            Ok(n) => n,
            Err(e) => {
                self.rotate_retry_at = t + ROTATE_RETRY_US;
                self.log_line(&format!("rotation to part {next_no} failed: {e}"));
                return;
            }
        };
        let next_name = file_name_of(&new.1).unwrap_or_default();
        self.marker(
            t,
            &CastMarker::Rotate(RotateInfo {
                part: next_no - 1,
                next: next_name,
                reason,
                extra: Extra::new(),
            }),
        );
        self.close(false);
        let mut info = self.template_at(t);
        info.continues = old_name;
        self.start_part(new, next_no, t, info);
    }

    fn open_part(&mut self, no: u32, t: u64, info: SessionInfo) {
        match self.factory.create(no) {
            Ok(created) => self.start_part(created, no, t, info),
            Err(e) => {
                self.last_part = self.last_part.max(no);
                self.degrade_unopened(&e, None);
            }
        }
    }

    fn start_part(
        &mut self,
        (mut w, path): (Box<dyn CastWriter + Send>, PathBuf),
        no: u32,
        t: u64,
        mut info: SessionInfo,
    ) {
        info.part = no;
        let epoch_s = info.start_epoch_us.div_euclid(1_000_000);
        let h = &self.header;
        let header = Header::new_v3(h.cols, h.rows, h.term.clone(), epoch_s, h.shell.clone());
        let pending = self
            .pending_error
            .as_ref()
            .map(|e| CastMarker::Error(e.clone()).to_json());
        let res = w
            .write_header(&header)
            .and_then(|()| w.write_event(0, EventCode::Marker, &CastMarker::Session(Box::new(info)).to_json()))
            .and_then(|()| pending.map_or(Ok(()), |m| w.write_event(0, EventCode::Marker, &m)))
            .and_then(|()| w.flush());
        self.last_part = no;
        self.last_name = Some(file_name_of(&path).unwrap_or_default());
        if let Err(e) = res {
            self.degrade_unopened(&e, Some(&path));
            return;
        }
        self.pending_error = None;
        if let Some(reg) = &self.registry {
            reg.set_cast(path.clone(), no);
        }
        self.part = Some(Part {
            w,
            path,
            no,
            t0: t,
            dirty: true,
            last_sync: t,
        });
        self.rotate_retry_at = 0;
        if self.shared.degraded.swap(false, Ordering::AcqRel)
            && let Some(reg) = &self.registry
        {
            reg.set_degraded(None);
        }
    }

    /// Writes an `error` marker if still possible, closes the part and degrades.
    fn fail(&mut self, t: u64, e: &CastError) {
        if let Some(mut part) = self.part.take() {
            let info = error_info(e);
            let rel = t.saturating_sub(part.t0);
            let written = part
                .w
                .write_event(rel, EventCode::Marker, &CastMarker::Error(info.clone()).to_json())
                .and_then(|()| part.w.flush())
                .and_then(|()| part.w.sync())
                .is_ok();
            if written {
                self.pending_error = None;
            } else {
                self.pending_error.get_or_insert(info);
            }
            let path = part.path.clone();
            drop(part);
            self.degrade(e, Some(&path));
        } else {
            self.degrade_unopened(e, None);
        }
    }

    /// Enters degraded mode after a part could not be created or started: its `error`
    /// marker waits for the continuation part.
    fn degrade_unopened(&mut self, e: &CastError, path: Option<&Path>) {
        self.pending_error.get_or_insert_with(|| error_info(e));
        self.degrade(e, path);
    }

    fn degrade(&mut self, e: &CastError, path: Option<&Path>) {
        let reason = format!("{}: {e}", error_code(e));
        self.shared.degraded.store(true, Ordering::Release);
        if let Some(reg) = &self.registry {
            reg.set_degraded(Some(reason.clone()));
        }
        let at = path.map(|p| format!(" ({})", p.display())).unwrap_or_default();
        self.log_line(&format!("degraded{at}: {reason}"));
    }

    fn close(&mut self, final_close: bool) {
        if final_close && !self.exited {
            self.flush_utf8_tails(self.clock.boottime_us());
        }
        if let Some(mut part) = self.part.take()
            && let Err(e) = part.w.sync()
        {
            self.log_line(&format!("closing {}: {e}", part.path.display()));
        }
    }

    fn log_line(&self, msg: &str) {
        if let Some(log) = &self.log {
            let _ = log.append(msg);
        }
    }
}

fn file_name_of(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::events::{Payload, channel};
    use cinelog_proto::{Attribution, Scope};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    /// Clock under test control.
    #[derive(Debug, Default)]
    pub(crate) struct FakeClock {
        pub boot: AtomicU64,
        pub epoch_offset: AtomicU64,
    }

    impl Clock for FakeClock {
        fn boottime_us(&self) -> u64 {
            self.boot.load(Ordering::SeqCst)
        }
        fn realtime_us(&self) -> i64 {
            1_731_070_000_000_000
                + self.boot.load(Ordering::SeqCst) as i64
                + self.epoch_offset.load(Ordering::SeqCst) as i64
        }
    }

    /// In-memory cast "files"; `fail_after` makes writes fail with ENOSPC after N bytes.
    #[derive(Default)]
    pub(crate) struct MemFiles {
        pub files: Mutex<Vec<(String, Vec<u8>)>>,
        pub fail_after: Mutex<Option<u64>>,
        pub fail_create: AtomicBool,
        pub delay: Mutex<Option<Duration>>,
    }

    struct MemWriter {
        files: Arc<MemFiles>,
        idx: usize,
        inner: cinelog_cast::V3Writer<Vec<u8>>,
        synced: usize,
    }

    impl MemWriter {
        fn check(&self) -> Result<(), CastError> {
            if let Some(d) = *self.files.delay.lock().unwrap() {
                std::thread::sleep(d);
            }
            match *self.files.fail_after.lock().unwrap() {
                Some(n) if self.inner.bytes_written() >= n => {
                    Err(CastError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)))
                }
                _ => Ok(()),
            }
        }
        fn publish(&mut self) {
            let buf = self.inner.get_ref();
            let mut files = self.files.files.lock().unwrap();
            files[self.idx].1 = buf.clone();
            self.synced = buf.len();
        }
    }

    impl CastWriter for MemWriter {
        fn write_header(&mut self, header: &Header) -> Result<(), CastError> {
            self.check()?;
            self.inner.write_header(header)?;
            self.publish();
            Ok(())
        }
        fn write_event(&mut self, t_us: u64, code: EventCode, data: &str) -> Result<(), CastError> {
            self.check()?;
            self.inner.write_event(t_us, code, data)?;
            self.publish();
            Ok(())
        }
        fn flush(&mut self) -> Result<(), CastError> {
            Ok(())
        }
        fn sync(&mut self) -> Result<(), CastError> {
            Ok(())
        }
        fn bytes_written(&self) -> u64 {
            self.inner.bytes_written()
        }
    }

    pub(crate) struct MemFactory(pub Arc<MemFiles>);

    impl CastFactory for MemFactory {
        fn file_name(&self, part: u32) -> String {
            if part <= 1 {
                "s.cast".into()
            } else {
                format!("s_part{part}.cast")
            }
        }
        fn create(&mut self, part: u32) -> Result<(Box<dyn CastWriter + Send>, PathBuf), CastError> {
            if self.0.fail_create.load(Ordering::SeqCst) {
                return Err(CastError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
            }
            let name = self.file_name(part);
            let mut files = self.0.files.lock().unwrap();
            files.push((name.clone(), Vec::new()));
            let idx = files.len() - 1;
            Ok((
                Box::new(MemWriter {
                    files: Arc::clone(&self.0),
                    idx,
                    inner: cinelog_cast::V3Writer::new(Vec::new()),
                    synced: 0,
                }),
                PathBuf::from("/mem").join(name),
            ))
        }
    }

    pub(crate) fn template() -> SessionInfo {
        SessionInfo {
            uuid: "0193-test".into(),
            part: 1,
            continues: None,
            start_epoch_us: 1_731_070_000_000_000,
            boottime_us: 0,
            tz: "UTC".into(),
            tz_offset_s: 0,
            scope: Scope::Personal,
            host: "box".into(),
            user: "alice".into(),
            uid: 1000,
            euid: 1000,
            sudo_user: None,
            claimed_user: None,
            attribution: Attribution::Ancestor,
            login_via: None,
            tty: "/dev/pts/1".into(),
            holder_comm: None,
            holder_pid: None,
            rec_pid: 1,
            rec_starttime: 1,
            boot_id: "b".into(),
            shell_pid: 2,
            tmux: None,
            parent_uuid: None,
            input: true,
            rec_version: "test".into(),
            protocol: 1,
            extra: Extra::new(),
        }
    }

    pub(crate) fn header() -> HeaderInfo {
        HeaderInfo {
            cols: 80,
            rows: 24,
            term: Some("xterm".into()),
            shell: Some("/bin/zsh".into()),
        }
    }

    /// Parsed lines of an in-memory cast: (interval µs, code, data).
    pub(crate) fn lines(buf: &[u8]) -> Vec<(u64, String, String)> {
        std::str::from_utf8(buf)
            .unwrap()
            .lines()
            .skip(1)
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                let t = (v[0].as_f64().unwrap() * 1e6).round() as u64;
                (t, v[1].as_str().unwrap().into(), v[2].as_str().unwrap().into())
            })
            .collect()
    }

    pub(crate) fn kinds(buf: &[u8]) -> Vec<String> {
        lines(buf)
            .into_iter()
            .map(|(_, c, d)| {
                if c == "m" {
                    let v: Value = serde_json::from_str(&d).unwrap();
                    format!("m:{}", v["k"].as_str().unwrap())
                } else {
                    format!("{c}:{d}")
                }
            })
            .collect()
    }

    pub(crate) fn exit(status: i32) -> Payload {
        Payload::Exit {
            status,
            bad_markers: 0,
            dropped_bytes: 0,
        }
    }

    fn cfg() -> WriterConfig {
        WriterConfig {
            rotate_bytes: 0,
            rotate_age_us: 0,
            fsync_us: 5_000_000,
        }
    }

    fn spawn(
        files: &Arc<MemFiles>,
        cfg: WriterConfig,
        clock: Arc<FakeClock>,
    ) -> (
        crate::events::EventTx,
        std::thread::JoinHandle<WriterStats>,
        Arc<WriterShared>,
    ) {
        let (tx, rx) = channel(crate::events::QUEUE_BUDGET);
        let shared = Arc::new(WriterShared::default());
        let w = Writer::new(
            Box::new(MemFactory(Arc::clone(files))),
            clock,
            template(),
            header(),
            cfg,
            Arc::clone(&shared),
            None,
            None,
            None,
        );
        (tx, std::thread::spawn(move || w.run(rx, 0)), shared)
    }

    #[test]
    fn writes_header_session_events_and_exit() {
        let files = Arc::new(MemFiles::default());
        let (tx, h, _) = spawn(&files, cfg(), Arc::new(FakeClock::default()));
        tx.data(1_000, Payload::Output("h\u{e9}".as_bytes()[..2].to_vec()));
        tx.data(1_500, Payload::Output(vec![0xa9, b'!']));
        tx.data(2_000, Payload::Input(b"ls\r".to_vec()));
        tx.always(3_000, Payload::Resize { cols: 100, rows: 30 });
        tx.always(4_000, exit(0));
        drop(tx);
        let stats = h.join().unwrap();
        let files = files.files.lock().unwrap();
        assert_eq!(files.len(), 1);
        let buf = &files[0].1;
        let head = std::str::from_utf8(buf).unwrap().lines().next().unwrap();
        assert!(
            head.starts_with(r#"{"version":3,"term":{"cols":80,"rows":24"#),
            "{head}"
        );
        assert_eq!(
            kinds(buf),
            ["m:session", "o:h", "o:\u{e9}!", "i:ls\r", "r:100x30", "m:stats", "x:0"],
            "a character split across reads is completed in the next event"
        );
        let l = lines(buf);
        assert_eq!(
            (l[1].0, l[2].0),
            (1_000, 500),
            "intervals relative to the previous event"
        );
        assert_eq!(
            stats,
            WriterStats {
                lossy_bytes: 0,
                parts: 1
            }
        );
    }

    fn spawn_scrubbing(
        files: &Arc<MemFiles>,
        clock: Arc<FakeClock>,
        nonce: Nonce,
    ) -> (crate::events::EventTx, std::thread::JoinHandle<WriterStats>) {
        let (tx, rx) = channel(crate::events::QUEUE_BUDGET);
        let w = Writer::new(
            Box::new(MemFactory(Arc::clone(files))),
            clock,
            template(),
            header(),
            cfg(),
            Arc::new(WriterShared::default()),
            None,
            None,
            Some(nonce),
        );
        (tx, std::thread::spawn(move || w.run(rx, 0)))
    }

    fn raw_of(files: &MemFiles) -> String {
        let files = files.files.lock().unwrap();
        files
            .first()
            .map(|f| String::from_utf8(f.1.clone()).unwrap())
            .unwrap_or_default()
    }

    /// Waits until the writer has written `needle` and gone idle (it flushes on idle).
    fn wait_idle_with(files: &MemFiles, needle: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !raw_of(files).contains(needle) {
            assert!(std::time::Instant::now() < deadline, "writer never wrote {needle:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn nonce_is_scrubbed_even_across_reads() {
        let files = Arc::new(MemFiles::default());
        let nonce = Nonce::from_bytes([0xc3; 16]);
        let hex = nonce.to_hex();
        let (tx, h) = spawn_scrubbing(&files, Arc::new(FakeClock::default()), nonce);
        tx.data(1, Payload::Output(format!("{{\"nonce\":\"{hex}\"}} cafe").into_bytes()));
        tx.data(2, Payload::Output(format!("x {}", &hex[..20]).into_bytes()));
        // The writer drains and idles between the two reads (regression: an idle flush
        // used to write the held half out).
        wait_idle_with(&files, "cafex ");
        tx.data(3, Payload::Output(format!("{} y", &hex[20..]).into_bytes()));
        tx.data(4, Payload::Input(hex.clone().into_bytes()));
        tx.marker(
            5,
            CastMarker::Unknown {
                k: "x".into(),
                fields: [("v".to_owned(), Value::from(hex.clone()))].into_iter().collect(),
            },
        );
        tx.data(6, Payload::Output(b"deadbeef".to_vec()));
        tx.always(7, exit(0));
        drop(tx);
        h.join().unwrap();
        let raw = raw_of(&files);
        assert!(!raw.contains(&hex), "{raw}");
        let files = files.files.lock().unwrap();
        let l = lines(&files[0].1);
        let out: String = l
            .iter()
            .filter(|(_, c, _)| c == "o")
            .map(|(_, _, d)| d.as_str())
            .collect();
        assert_eq!(out, "{\"nonce\":\"<cinelog-nonce>\"} cafex <cinelog-nonce> ydeadbeef");
        // A held hex tail keeps its own timestamp when it is released at the next event.
        let deadbeef = l.iter().position(|(_, _, d)| d == "deadbeef").unwrap();
        assert_eq!(l[deadbeef + 1].1, "m", "released right before the stats marker");
    }

    #[test]
    fn held_hex_tail_is_released_after_the_hold_bound() {
        let files = Arc::new(MemFiles::default());
        let clock = Arc::new(FakeClock::default());
        clock.boot.store(1_000, Ordering::SeqCst);
        let (tx, h) = spawn_scrubbing(&files, Arc::clone(&clock), Nonce::from_bytes([0xc3; 16]));
        tx.data(1_000, Payload::Output(b"id deadbeef".to_vec()));
        wait_idle_with(&files, "id ");
        assert!(
            !raw_of(&files).contains("deadbeef"),
            "held while it may still be a nonce"
        );
        clock.boot.store(1_000 + HOLD_US, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !raw_of(&files).contains("deadbeef") {
            assert!(std::time::Instant::now() < deadline, "held tail never released");
            std::thread::sleep(Duration::from_millis(5));
        }
        tx.always(2_000_000, exit(0));
        drop(tx);
        h.join().unwrap();
        let files = files.files.lock().unwrap();
        let l = lines(&files[0].1);
        let at = l.iter().position(|(_, _, d)| d == "deadbeef").unwrap();
        assert_eq!(l[at].0, 0, "written with the timestamp of its read");
    }

    #[test]
    fn invalid_utf8_is_counted() {
        let files = Arc::new(MemFiles::default());
        let (tx, h, _) = spawn(&files, cfg(), Arc::new(FakeClock::default()));
        tx.data(1, Payload::Output(vec![b'a', 0xff, b'b']));
        tx.data(2, Payload::Output(vec![0xe2, 0x82]));
        tx.always(3, exit(1));
        drop(tx);
        let stats = h.join().unwrap();
        assert_eq!(stats.lossy_bytes, 3);
        let files = files.files.lock().unwrap();
        let k = kinds(&files[0].1);
        assert_eq!(k[1], "o:a\u{fffd}b");
        assert!(k.contains(&"x:1".to_owned()));
    }

    #[test]
    fn rotates_by_size_and_links_parts() {
        let files = Arc::new(MemFiles::default());
        let c = WriterConfig {
            rotate_bytes: 600,
            ..cfg()
        };
        let clock = Arc::new(FakeClock::default());
        let (tx, h, _) = spawn(&files, c, Arc::clone(&clock));
        for i in 0..20u64 {
            clock.boot.store(i * 1_000_000, Ordering::SeqCst);
            tx.data(i * 1_000_000, Payload::Output(vec![b'x'; 100]));
        }
        tx.always(20_000_000, exit(0));
        drop(tx);
        let stats = h.join().unwrap();
        let files = files.files.lock().unwrap();
        assert!(files.len() >= 3, "{}", files.len());
        assert_eq!(stats.parts as usize, files.len());
        let first = kinds(&files[0].1);
        assert_eq!(first.last().unwrap(), "m:rotate");
        let l = lines(&files[0].1);
        let rot: Value = serde_json::from_str(&l.last().unwrap().2).unwrap();
        assert_eq!(rot["next"], "s_part2.cast");
        assert_eq!(rot["reason"], "size");
        let l2 = lines(&files[1].1);
        let s2: Value = serde_json::from_str(&l2[0].2).unwrap();
        assert_eq!((s2["k"].as_str(), s2["part"].as_u64()), (Some("session"), Some(2)));
        assert_eq!(s2["continues"], "s.cast");
        assert_eq!(s2["uuid"], "0193-test", "all parts share the uuid");
        // Offsets restart at the part's t=0.
        assert!(l2[1].0 <= 1_000_000, "{:?}", l2[1]);
        assert!(kinds(&files.last().unwrap().1).ends_with(&["m:stats".into(), "x:0".into()]));
    }

    #[test]
    fn rotates_by_age_while_idle() {
        let files = Arc::new(MemFiles::default());
        let c = WriterConfig {
            rotate_age_us: 10_000_000,
            ..cfg()
        };
        let clock = Arc::new(FakeClock::default());
        let (tx, h, _) = spawn(&files, c, Arc::clone(&clock));
        tx.data(1, Payload::Output(b"a".to_vec()));
        clock.boot.store(11_000_000, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while files.files.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        tx.always(11_500_000, exit(0));
        drop(tx);
        h.join().unwrap();
        let files = files.files.lock().unwrap();
        assert_eq!(files.len(), 2);
        let l = lines(&files[0].1);
        let rot: Value = serde_json::from_str(&l.last().unwrap().2).unwrap();
        assert_eq!(rot["reason"], "age");
        let s2: Value = serde_json::from_str(&lines(&files[1].1)[0].2).unwrap();
        assert_eq!(s2["boottime_us"], 11_000_000);
        assert_eq!(s2["start_epoch_us"], 1_731_070_011_000_000i64);
    }

    #[test]
    fn enospc_degrades_and_resume_opens_the_next_part() {
        let files = Arc::new(MemFiles::default());
        let (tx, h, shared) = spawn(&files, cfg(), Arc::new(FakeClock::default()));
        tx.data(1, Payload::Output(b"before".to_vec()));
        let wait = |cond: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !cond() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&|| {
            files
                .files
                .lock()
                .unwrap()
                .first()
                .is_some_and(|f| kinds(&f.1).len() == 2)
        });
        *files.fail_after.lock().unwrap() = Some(0);
        tx.data(2, Payload::Output(b"lost".to_vec()));
        wait(&|| shared.degraded.load(Ordering::SeqCst));
        assert!(shared.degraded.load(Ordering::SeqCst));
        tx.data(3, Payload::Output(b"while degraded".to_vec()));
        *files.fail_after.lock().unwrap() = None;
        tx.control(Msg::ResumeDegraded { t: 4 });
        tx.data(5, Payload::Output(b"after".to_vec()));
        tx.always(6, exit(0));
        drop(tx);
        let stats = h.join().unwrap();
        assert!(!shared.degraded.load(Ordering::SeqCst));
        let files = files.files.lock().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            kinds(&files[0].1),
            ["m:session", "o:before"],
            "error marker could not be written"
        );
        let k2 = kinds(&files[1].1);
        assert_eq!(k2, ["m:session", "m:error", "o:after", "m:stats", "x:0"]);
        let err: Value = serde_json::from_str(&lines(&files[1].1)[1].2).unwrap();
        assert_eq!(
            (err["error"].as_str(), err["degraded"].as_bool()),
            (Some("enospc"), Some(true))
        );
        let s2: Value = serde_json::from_str(&lines(&files[1].1)[0].2).unwrap();
        assert_eq!(
            (s2["part"].as_u64(), s2["continues"].as_str()),
            (Some(2), Some("s.cast"))
        );
        assert_eq!(stats.parts, 2);
    }

    #[test]
    fn create_failure_at_start_degrades() {
        let files = Arc::new(MemFiles::default());
        files.fail_create.store(true, Ordering::SeqCst);
        let (tx, h, shared) = spawn(&files, cfg(), Arc::new(FakeClock::default()));
        tx.data(1, Payload::Output(b"x".to_vec()));
        tx.always(2, exit(0));
        drop(tx);
        h.join().unwrap();
        assert!(shared.degraded.load(Ordering::SeqCst));
        assert!(files.files.lock().unwrap().is_empty());
    }

    #[test]
    fn gap_marker_uses_part_offsets() {
        let files = Arc::new(MemFiles::default());
        let (tx, h, _) = spawn(&files, cfg(), Arc::new(FakeClock::default()));
        tx.control(Msg::Gap {
            dropped_bytes: 42,
            from: 1_000_000,
            to: 2_500_000,
        });
        tx.always(3_000_000, exit(0));
        drop(tx);
        h.join().unwrap();
        let files = files.files.lock().unwrap();
        let l = lines(&files[0].1);
        let g: Value = serde_json::from_str(&l[1].2).unwrap();
        assert_eq!(g["k"], "gap");
        assert_eq!(g["dropped_bytes"], 42);
        assert_eq!((g["from_t"].as_f64(), g["to_t"].as_f64()), (Some(1.0), Some(2.5)));
    }

    #[test]
    fn slow_writer_never_blocks_senders_and_reports_the_gap() {
        let files = Arc::new(MemFiles::default());
        *files.delay.lock().unwrap() = Some(Duration::from_millis(30));
        let (tx, rx) = channel(64 * 1024);
        let shared = Arc::new(WriterShared::default());
        let w = Writer::new(
            Box::new(MemFactory(Arc::clone(&files))),
            Arc::new(FakeClock::default()),
            template(),
            header(),
            cfg(),
            shared,
            None,
            None,
            None,
        );
        let h = std::thread::spawn(move || w.run(rx, 0));
        let start = std::time::Instant::now();
        let mut accepted = 0;
        for i in 0..200u64 {
            if tx.data(i, Payload::Output(vec![b'y'; 4096])) {
                accepted += 1;
            }
        }
        assert!(start.elapsed() < Duration::from_millis(500), "senders must not block");
        assert!(accepted < 200);
        assert_eq!(tx.dropped_total(), (200 - accepted) * 4096);
        *files.delay.lock().unwrap() = None;
        // Once the writer has caught up the next event gets through, preceded by the gap.
        while !tx.data(1000, Payload::Output(b"tail".to_vec())) {
            std::thread::sleep(Duration::from_millis(10));
        }
        tx.always(1001, exit(0));
        drop(tx);
        h.join().unwrap();
        let files = files.files.lock().unwrap();
        let k = kinds(&files[0].1);
        let gap = k.iter().rposition(|k| k == "m:gap").expect("gap marker");
        assert_eq!(k[gap + 1], "o:tail");
    }
}
