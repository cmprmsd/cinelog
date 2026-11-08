//! `--since`/`--until` values: a date (`2026-11-01`), a local date and time
//! (`2026-11-01T14:00`, `2026-11-01 14:00:30`), an RFC 3339 instant (`…Z`, `…+01:00`), an
//! age (`90s`, `15m`, `3h`, `2d`, `1w`), `@<epoch>`, `now`, `today` or `yesterday`.

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// Which end of a range a value bounds. A bare date covers the whole day: as `Until` it
/// means the end of that day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Since,
    Until,
}

/// Parses `s` into UTC epoch seconds.
pub fn parse(s: &str, bound: Bound, now: Timestamp, tz: &TimeZone) -> Result<f64, String> {
    let s = s.trim();
    let bad = || format!("{s:?}: expected a date (2026-11-01), a date and time (2026-11-01T14:00) or an age (2d, 3h)");
    let ts = match s {
        "now" => now,
        "today" => day_bound(now.to_zoned(tz.clone()).date(), bound, tz)?,
        "yesterday" => {
            let today = now.to_zoned(tz.clone()).date();
            day_bound(today.yesterday().map_err(|e| e.to_string())?, bound, tz)?
        }
        _ => {
            if let Some(epoch) = s.strip_prefix('@') {
                let secs: f64 = epoch.parse().ok().filter(|v: &f64| v.is_finite()).ok_or_else(bad)?;
                return Ok(secs);
            }
            if let Some(age) = parse_age(s) {
                let age = age.ok_or_else(bad)?;
                now.checked_sub(age).map_err(|e| format!("{s:?}: {e}"))?
            } else if let Ok(ts) = s.parse::<Timestamp>() {
                ts
            } else if let Ok(dt) = s.parse::<DateTime>()
                && s.len() > 10
            {
                dt.to_zoned(tz.clone()).map_err(|e| format!("{s:?}: {e}"))?.timestamp()
            } else if let Ok(d) = s.parse::<Date>() {
                day_bound(d, bound, tz)?
            } else {
                return Err(bad());
            }
        }
    };
    Ok(ts.as_microsecond() as f64 / 1e6)
}

/// `Some(None)`: looks like an age but is malformed; `None`: not an age.
fn parse_age(s: &str) -> Option<Option<SignedDuration>> {
    let unit = s.chars().last()?;
    let secs = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86_400.0,
        'w' => 7.0 * 86_400.0,
        _ => return None,
    };
    let num = &s[..s.len() - 1];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(
        num.parse::<f64>()
            .ok()
            .map(|n| n * secs)
            .filter(|v| v.is_finite() && *v < 1e12)
            .map(SignedDuration::from_secs_f64),
    )
}

fn day_bound(d: Date, bound: Bound, tz: &TimeZone) -> Result<Timestamp, String> {
    let start = |d: Date| d.to_zoned(tz.clone()).map(|z| z.timestamp()).map_err(|e| e.to_string());
    match bound {
        Bound::Since => start(d),
        Bound::Until => {
            let next = start(d.tomorrow().map_err(|e| e.to_string())?)?;
            next.checked_sub(SignedDuration::from_micros(1))
                .map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str, bound: Bound) -> Result<f64, String> {
        // 2026-11-08T12:00:00Z
        let now = Timestamp::from_second(1_794_139_200).unwrap();
        parse(s, bound, now, &TimeZone::UTC)
    }

    const NOW: f64 = 1_794_139_200.0;

    #[test]
    fn ages() {
        assert_eq!(at("2d", Bound::Since).unwrap(), NOW - 2.0 * 86_400.0);
        assert_eq!(at("3h", Bound::Since).unwrap(), NOW - 3.0 * 3600.0);
        assert_eq!(at("15m", Bound::Until).unwrap(), NOW - 900.0);
        assert_eq!(at("90s", Bound::Since).unwrap(), NOW - 90.0);
        assert_eq!(at("1.5h", Bound::Since).unwrap(), NOW - 5400.0);
        assert_eq!(at("1w", Bound::Since).unwrap(), NOW - 7.0 * 86_400.0);
        assert_eq!(at("now", Bound::Until).unwrap(), NOW);
        assert!(at("1..2d", Bound::Since).is_err());
        assert!(at("d", Bound::Since).is_err());
        assert!(at("2x", Bound::Since).is_err());
    }

    #[test]
    fn dates_cover_the_whole_day() {
        let day = 1_793_491_200.0; // 2026-11-01T00:00:00Z
        assert_eq!(at("2026-11-01", Bound::Since).unwrap(), day);
        assert_eq!(at("2026-11-01", Bound::Until).unwrap(), day + 86_400.0 - 1e-6);
        assert_eq!(at("today", Bound::Since).unwrap(), 1_794_096_000.0);
        assert_eq!(at("yesterday", Bound::Until).unwrap(), 1_794_096_000.0 - 1e-6);
    }

    #[test]
    fn date_times_and_instants() {
        let day = 1_793_491_200.0;
        assert_eq!(at("2026-11-01T14:00", Bound::Since).unwrap(), day + 14.0 * 3600.0);
        assert_eq!(
            at("2026-11-01 14:00:30", Bound::Until).unwrap(),
            day + 14.0 * 3600.0 + 30.0
        );
        assert_eq!(
            at("2026-11-01T14:00:00+01:00", Bound::Since).unwrap(),
            day + 13.0 * 3600.0
        );
        assert_eq!(at("2026-11-01T14:00:00Z", Bound::Since).unwrap(), day + 14.0 * 3600.0);
        assert_eq!(at("@1700000000.5", Bound::Since).unwrap(), 1_700_000_000.5);
        assert!(at("2026-13-01", Bound::Since).is_err());
        assert!(at("@x", Bound::Since).is_err());
        assert!(at("", Bound::Since).is_err());
    }

    #[test]
    fn local_dates_use_the_given_zone() {
        let tz = TimeZone::fixed(jiff::tz::offset(2));
        let now = Timestamp::from_second(1_794_139_200).unwrap();
        let v = parse("2026-11-01", Bound::Since, now, &tz).unwrap();
        assert_eq!(v, 1_793_491_200.0 - 7200.0);
    }
}
