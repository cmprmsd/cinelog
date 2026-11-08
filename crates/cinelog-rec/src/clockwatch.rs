//! `clock` markers (plan §5.4): every 5 minutes, and at once when the wall clock moves
//! more than 2 s away from `CLOCK_BOOTTIME` since the last reference point.

use cinelog_core::clock::{CLOCK_MARKER_INTERVAL, CLOCK_STEP_THRESHOLD_US};
use cinelog_proto::{ClockReason, ClockSync, Extra};

#[derive(Debug, Clone)]
pub(crate) struct ClockWatch {
    ref_epoch: i64,
    ref_boot: u64,
    interval_us: u64,
}

impl ClockWatch {
    /// Reference point = the `session` marker's `(start_epoch_us, boottime_us)`.
    pub(crate) fn new(epoch_us: i64, boot_us: u64) -> Self {
        Self {
            ref_epoch: epoch_us,
            ref_boot: boot_us,
            interval_us: u64::try_from(CLOCK_MARKER_INTERVAL.as_micros()).unwrap_or(u64::MAX),
        }
    }

    pub(crate) fn tick(&mut self, epoch_us: i64, boot_us: u64) -> Option<ClockSync> {
        let elapsed = boot_us.saturating_sub(self.ref_boot);
        let expected = self
            .ref_epoch
            .saturating_add(i64::try_from(elapsed).unwrap_or(i64::MAX));
        let reason = if (epoch_us - expected).abs() > CLOCK_STEP_THRESHOLD_US {
            ClockReason::Step
        } else if elapsed >= self.interval_us {
            ClockReason::Periodic
        } else {
            return None;
        };
        self.ref_epoch = epoch_us;
        self.ref_boot = boot_us;
        Some(ClockSync {
            epoch_us,
            boottime_us: boot_us,
            reason,
            extra: Extra::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn periodic_and_step() {
        let e0 = 1_731_070_000_000_000i64;
        let mut w = ClockWatch::new(e0, 10 * S);
        assert!(w.tick(e0 + 1_000_000, 11 * S).is_none());
        // NTP slew below the threshold is not a step.
        assert!(w.tick(e0 + 61_500_000, 70 * S).is_none());
        let p = w.tick(e0 + 300_000_000, 310 * S).unwrap();
        assert_eq!(p.reason, ClockReason::Periodic);
        assert_eq!((p.epoch_us, p.boottime_us), (e0 + 300_000_000, 310 * S));
        // +1 h manual change: immediate step marker, then quiet again.
        let s = w.tick(e0 + 301_000_000 + 3_600_000_000, 311 * S).unwrap();
        assert_eq!(s.reason, ClockReason::Step);
        assert!(w.tick(e0 + 302_000_000 + 3_600_000_000, 312 * S).is_none());
        // Backwards step.
        let b = w.tick(e0, 313 * S).unwrap();
        assert_eq!(b.reason, ClockReason::Step);
    }

    #[test]
    fn suspend_is_not_a_step() {
        // CLOCK_BOOTTIME counts suspend, so wall clock and boottime advance together.
        let mut w = ClockWatch::new(0, 0);
        assert_eq!(
            w.tick(7_200 * S as i64, 7_200 * S).unwrap().reason,
            ClockReason::Periodic
        );
    }
}
