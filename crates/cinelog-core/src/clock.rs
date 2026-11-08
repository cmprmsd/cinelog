//! Clocks (§5.4): intervals use `CLOCK_BOOTTIME`, wall time `CLOCK_REALTIME`.
//!
//! With the `testhooks` feature, [`system_clock`] honours `CINELOG_TESTHOOK_CLOCK_FILE`:
//! a file holding a signed µs offset that is added to realtime and re-read on every call,
//! so a test can step the wall clock of a running recorder (I16).

use std::time::Duration;

/// Interval between periodic `clock` markers.
pub const CLOCK_MARKER_INTERVAL: Duration = Duration::from_secs(300);
/// `|Δrealtime − Δboottime|` above this triggers an immediate `clock` marker.
pub const CLOCK_STEP_THRESHOLD_US: i64 = 2_000_000;

pub trait Clock: Send + Sync {
    /// Monotonic µs including suspend.
    fn boottime_us(&self) -> u64;
    /// Wall clock, µs since the Unix epoch.
    fn realtime_us(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

fn clock_us(id: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid clock id and a properly sized out-pointer.
    let rc = unsafe { libc::clock_gettime(id, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime({id}) failed");
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
}

impl Clock for SystemClock {
    fn boottime_us(&self) -> u64 {
        u64::try_from(clock_us(libc::CLOCK_BOOTTIME)).unwrap_or(0)
    }

    fn realtime_us(&self) -> i64 {
        clock_us(libc::CLOCK_REALTIME)
    }
}

#[cfg(feature = "testhooks")]
pub const TESTHOOK_CLOCK_FILE: &str = "CINELOG_TESTHOOK_CLOCK_FILE";

#[cfg(feature = "testhooks")]
#[derive(Debug, Clone)]
pub struct OffsetFileClock {
    pub path: std::path::PathBuf,
}

#[cfg(feature = "testhooks")]
impl Clock for OffsetFileClock {
    fn boottime_us(&self) -> u64 {
        SystemClock.boottime_us()
    }

    fn realtime_us(&self) -> i64 {
        let offset = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        SystemClock.realtime_us() + offset
    }
}

/// The clock the recorder should use.
pub fn system_clock() -> Box<dyn Clock> {
    #[cfg(feature = "testhooks")]
    if let Some(path) = std::env::var_os(TESTHOOK_CLOCK_FILE) {
        return Box::new(OffsetFileClock { path: path.into() });
    }
    Box::new(SystemClock)
}

/// Local time zone at `epoch_us`: IANA name (or `"unknown"`) and UTC offset in seconds.
pub fn local_tz(epoch_us: i64) -> (String, i32) {
    let tz = jiff::tz::TimeZone::system();
    let name = tz.iana_name().unwrap_or("unknown").to_owned();
    let offset = jiff::Timestamp::from_microsecond(epoch_us)
        .map(|ts| tz.to_offset(ts).seconds())
        .unwrap_or(0);
    (name, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_sane() {
        let c = SystemClock;
        let (b1, r1) = (c.boottime_us(), c.realtime_us());
        assert!(r1 > 1_600_000_000_000_000);
        assert!(c.boottime_us() >= b1);
    }

    #[test]
    fn local_tz_has_a_name() {
        let (name, offset) = local_tz(SystemClock.realtime_us());
        assert!(!name.is_empty());
        assert!(offset.abs() <= 14 * 3600);
    }
}
