//! Cast offset → wall clock mapping from `session` and `clock` markers (§5.4).

use serde::Deserialize;

use crate::{Event, EventCode, Header};

/// A known (cast offset, wall clock) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAnchor {
    pub t_us: u64,
    /// µs since the Unix epoch (UTC).
    pub epoch_us: i64,
    /// The wall clock stepped (NTP step, manual change) right before this anchor, so the
    /// jump is not smeared over the preceding piece.
    pub step: bool,
}

/// Maps cast offsets to epoch µs, piecewise linear between anchors.
///
/// Between two anchors the mapping interpolates linearly (absorbing clock slew), except
/// when the later anchor is a step: then the earlier piece runs at slope 1 and the step
/// happens at the anchor. Before the first and after the last anchor the slope is 1, so a
/// live cast can be mapped with the anchors seen so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClockMap {
    anchors: Vec<ClockAnchor>,
}

#[derive(Deserialize)]
struct MarkerTimes<'a> {
    /// Present on every cinelog marker; `m` events without it are foreign labels.
    cl: Option<u64>,
    #[serde(borrow)]
    k: &'a str,
    start_epoch_us: Option<i64>,
    epoch_us: Option<i64>,
    reason: Option<&'a str>,
}

impl ClockMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// A map with a single anchor at t=0.
    pub fn with_start(start_epoch_us: i64) -> Self {
        let mut map = Self::new();
        map.insert(ClockAnchor {
            t_us: 0,
            epoch_us: start_epoch_us,
            step: false,
        });
        map
    }

    /// Fallback from the header `timestamp` (whole seconds), for casts without a `session`
    /// marker (legacy, third-party). A `session` marker at t=0 replaces it.
    pub fn from_header(header: &Header) -> Self {
        header
            .timestamp
            .map_or_else(Self::new, |ts| Self::with_start(ts.saturating_mul(1_000_000)))
    }

    /// Adds an anchor; one at an already known offset replaces the earlier one.
    pub fn insert(&mut self, anchor: ClockAnchor) {
        match self.anchors.binary_search_by_key(&anchor.t_us, |a| a.t_us) {
            Ok(i) => self.anchors[i] = anchor,
            Err(i) => self.anchors.insert(i, anchor),
        }
    }

    /// Feeds an event; `session` and `clock` markers become anchors. Returns whether the
    /// event added an anchor.
    pub fn observe(&mut self, ev: &Event) -> bool {
        if ev.code != EventCode::Marker || !(ev.data.contains("\"session\"") || ev.data.contains("\"clock\"")) {
            return false;
        }
        let Ok(m @ MarkerTimes { cl: Some(_), .. }) = serde_json::from_str::<MarkerTimes<'_>>(&ev.data) else {
            return false;
        };
        let (epoch_us, step) = match m.k {
            "session" => (m.start_epoch_us, false),
            "clock" => (m.epoch_us, m.reason == Some("step")),
            _ => return false,
        };
        let Some(epoch_us) = epoch_us else { return false };
        self.insert(ClockAnchor {
            t_us: ev.t_us,
            epoch_us,
            step,
        });
        true
    }

    pub fn anchors(&self) -> &[ClockAnchor] {
        &self.anchors
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    /// Wall clock at cast offset `t_us`, or `None` without any anchor.
    pub fn epoch_us(&self, t_us: u64) -> Option<i64> {
        let idx = self.anchors.partition_point(|a| a.t_us <= t_us);
        let Some(a) = idx.checked_sub(1).map(|i| self.anchors[i]) else {
            let first = self.anchors.first()?;
            return Some(first.epoch_us - delta(t_us, first.t_us));
        };
        let elapsed = delta(a.t_us, t_us);
        Some(match self.anchors.get(idx) {
            Some(b) if !b.step => {
                let span = i128::from(delta(a.t_us, b.t_us));
                let gain = i128::from(b.epoch_us) - i128::from(a.epoch_us);
                a.epoch_us + (i128::from(elapsed) * gain / span) as i64
            }
            _ => a.epoch_us + elapsed,
        })
    }

    /// [`Self::epoch_us`] in seconds (the DB's REAL timestamps).
    pub fn epoch_secs(&self, t_us: u64) -> Option<f64> {
        self.epoch_us(t_us).map(|us| us as f64 / 1e6)
    }
}

/// `to - from` in µs as i64 (offsets are far below `i64::MAX`).
fn delta(from: u64, to: u64) -> i64 {
    (i128::from(to) - i128::from(from)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(t_us: u64, data: &str) -> Event {
        Event {
            t_us,
            code: EventCode::Marker,
            data: data.into(),
        }
    }

    #[test]
    fn empty_map() {
        assert_eq!(ClockMap::new().epoch_us(5), None);
    }

    #[test]
    fn single_anchor_is_slope_one() {
        let m = ClockMap::with_start(1_000_000_000);
        assert_eq!(m.epoch_us(0), Some(1_000_000_000));
        assert_eq!(m.epoch_us(2_500_000), Some(1_002_500_000));
    }

    #[test]
    fn interpolates_between_periodic_markers() {
        let mut m = ClockMap::new();
        assert!(m.observe(&marker(
            0,
            r#"{"cl":1,"k":"session","uuid":"u","start_epoch_us":1000000000}"#
        )));
        // 300 s later the wall clock gained 3 ms (slewed).
        assert!(m.observe(&marker(
            300_000_000,
            r#"{"cl":1,"k":"clock","epoch_us":1300003000,"boottime_us":5,"reason":"periodic"}"#
        )));
        assert_eq!(m.epoch_us(150_000_000), Some(1_150_001_500));
        assert_eq!(m.epoch_us(300_000_000), Some(1_300_003_000));
        assert_eq!(m.epoch_us(301_000_000), Some(1_301_003_000));
        assert_eq!(m.epoch_secs(0), Some(1000.0));
    }

    #[test]
    fn step_is_not_smeared() {
        let mut m = ClockMap::with_start(1_000_000_000);
        m.observe(&marker(
            100_000_000,
            r#"{"cl":1,"k":"clock","epoch_us":1160000000,"boottime_us":1,"reason":"step"}"#,
        ));
        assert_eq!(m.epoch_us(99_000_000), Some(1_099_000_000));
        assert_eq!(m.epoch_us(100_000_000), Some(1_160_000_000));
        assert_eq!(m.epoch_us(101_000_000), Some(1_161_000_000));
    }

    #[test]
    fn header_fallback_is_replaced_by_session() {
        let h = Header::new_v3(80, 24, None, 1_700_000_000, None);
        let mut m = ClockMap::from_header(&h);
        assert_eq!(m.epoch_us(0), Some(1_700_000_000_000_000));
        m.observe(&marker(
            0,
            r#"{"cl":1,"k":"session","start_epoch_us":1700000000123456}"#,
        ));
        assert_eq!(m.anchors().len(), 1);
        assert_eq!(m.epoch_us(1), Some(1_700_000_000_123_457));
    }

    #[test]
    fn before_first_anchor_and_ignored_events() {
        let mut m = ClockMap::new();
        m.insert(ClockAnchor {
            t_us: 10_000_000,
            epoch_us: 50_000_000,
            step: false,
        });
        assert_eq!(m.epoch_us(4_000_000), Some(44_000_000));
        assert!(!m.observe(&marker(1, r#"{"cl":1,"k":"cmd","typed":"echo \"clock\""}"#)));
        assert!(!m.observe(&marker(1, r#"{"cl":1,"k":"clock"}"#)));
        assert!(!m.observe(&marker(1, "not json \"clock\"")));
        assert!(!m.observe(&marker(1, r#"{"k":"clock","epoch_us":1}"#)));
        assert!(!m.observe(&Event {
            t_us: 1,
            code: EventCode::Output,
            data: r#"{"k":"clock","epoch_us":1}"#.into()
        }));
        assert_eq!(m.anchors().len(), 1);
    }
}
