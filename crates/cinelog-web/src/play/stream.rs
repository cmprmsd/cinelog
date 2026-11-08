//! What every served event stream does to the cast's events (docs/API.md §5.6, §6.1):
//! `cmd` markers become `$ <typed>` labels, every other marker is dropped, and forgotten
//! spans are masked: their output is replaced by one `[redacted by <who>]` output event,
//! their input and labels are dropped. Resizes and the exit event pass through.

use std::collections::HashSet;
use std::fmt::Write as _;

use cinelog_cast::{Event, EventCode};
use cinelog_proto::CastMarker;

/// A forgotten span in µs of session time, inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub from_us: u64,
    /// `u64::MAX`: a forgotten command that has not ended.
    pub to_us: u64,
    pub by: String,
}

/// Everything forgotten in one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Mask {
    /// By `from_us`.
    pub spans: Vec<Span>,
    /// Forgotten command seqs (their labels never appear, wherever their span is).
    pub seqs: HashSet<u64>,
}

impl Mask {
    pub fn new(mut spans: Vec<Span>, seqs: HashSet<u64>) -> Self {
        spans.sort_by_key(|s| (s.from_us, s.to_us));
        spans.dedup();
        Self { spans, seqs }
    }

    /// The first span containing `t`.
    pub fn span_at(&self, t_us: u64) -> Option<usize> {
        self.spans
            .iter()
            .take_while(|s| s.from_us <= t_us)
            .position(|s| t_us <= s.to_us)
    }

    pub fn first_start(&self) -> Option<u64> {
        self.spans.first().map(|s| s.from_us)
    }

    /// Identity for caches.
    pub fn key(&self) -> String {
        let mut k = String::new();
        for s in &self.spans {
            let _ = write!(k, "{}-{}-{};", s.from_us, s.to_us, s.by);
        }
        let mut seqs: Vec<_> = self.seqs.iter().collect();
        seqs.sort();
        for s in seqs {
            let _ = write!(k, "#{s}");
        }
        k
    }
}

/// One event as served: absolute session time in µs, code `o`, `i`, `r`, `m` or `x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutEvent {
    pub t_us: u64,
    pub code: char,
    pub data: String,
}

impl OutEvent {
    pub fn new(t_us: u64, code: char, data: impl Into<String>) -> Self {
        Self {
            t_us,
            code,
            data: data.into(),
        }
    }
}

/// Resize data as served: clamped to [`super::source::MAX_COLS`] x
/// [`super::source::MAX_ROWS`] (players allocate every cell too); `None` for data that is
/// no size (or has a zero), which the server's terminal ignores and so the player must not
/// see either.
fn resize_data(data: &str) -> Option<String> {
    let (c, r) = super::source::parse_size(data)?;
    Some(cinelog_cast::format_resize(c, r))
}

/// The text that replaces a forgotten span's output.
pub(crate) fn placeholder(by: &str) -> String {
    format!("[redacted by {by}]\r\n")
}

/// The chapter label of a command.
pub(crate) fn label(typed: &str) -> String {
    format!("$ {typed}")
}

/// Applies the rewrite and the mask to a stream of cast events.
#[derive(Debug, Clone)]
pub(crate) struct Rewriter<'a> {
    mask: &'a Mask,
    /// Per span: its placeholder was emitted (in this stream, or before its start).
    shown: Vec<bool>,
    /// Turn `cmd` markers into labels (otherwise every marker is dropped and labels are
    /// merged in from the index).
    labels: bool,
}

impl<'a> Rewriter<'a> {
    pub fn new(mask: &'a Mask, shown: Vec<bool>, labels: bool) -> Self {
        let mut shown = shown;
        shown.resize(mask.spans.len(), false);
        Self { mask, shown, labels }
    }

    /// Placeholders emitted so far, per span.
    pub fn shown(&self) -> &[bool] {
        &self.shown
    }

    /// Rewrites one event; `None` when it is dropped.
    pub fn push(&mut self, ev: Event) -> Option<OutEvent> {
        let t = ev.t_us;
        if let Some(i) = self.mask.span_at(t) {
            return match ev.code {
                EventCode::Output if !self.shown[i] => {
                    self.shown[i] = true;
                    Some(OutEvent::new(t, 'o', placeholder(&self.mask.spans[i].by)))
                }
                EventCode::Resize => resize_data(&ev.data).map(|d| OutEvent::new(t, 'r', d)),
                EventCode::Exit => Some(OutEvent::new(t, 'x', ev.data)),
                _ => None,
            };
        }
        if ev.code == EventCode::Resize {
            return resize_data(&ev.data).map(|d| OutEvent::new(t, 'r', d));
        }
        let code = match ev.code {
            EventCode::Output => 'o',
            EventCode::Input => 'i',
            EventCode::Resize => 'r',
            EventCode::Exit => 'x',
            EventCode::Marker if self.labels => {
                return match CastMarker::from_json(&ev.data) {
                    Ok(Some(CastMarker::Cmd(rec))) if !self.mask.seqs.contains(&rec.seq) => {
                        Some(OutEvent::new(t, 'm', label(&rec.cmd.typed)))
                    }
                    _ => None,
                };
            }
            EventCode::Marker | EventCode::Other(_) => return None,
        };
        Some(OutEvent {
            t_us: t,
            code,
            data: ev.data,
        })
    }
}

/// Merges labels (ascending) into an event stream: every label is emitted before the first
/// event that is later than it.
#[derive(Debug)]
pub(crate) struct LabelMerge<'a> {
    labels: &'a [(u64, String)],
    next: usize,
}

impl<'a> LabelMerge<'a> {
    /// Labels from `from_us` on.
    pub fn new(labels: &'a [(u64, String)], from_us: u64) -> Self {
        let next = labels.partition_point(|(t, _)| *t < from_us);
        Self { labels, next }
    }

    /// Labels due before an event at `t_us` (all up to `t_us`, inclusive).
    pub fn until(&mut self, t_us: u64) -> impl Iterator<Item = OutEvent> + '_ {
        let start = self.next;
        let end = start + self.labels[start..].partition_point(|(t, _)| *t <= t_us);
        self.next = end;
        self.labels[start..end]
            .iter()
            .map(|(t, l)| OutEvent::new(*t, 'm', l.clone()))
    }

    /// Labels before `end_us` (exclusive), for the end of a segment.
    pub fn before(&mut self, end_us: u64) -> impl Iterator<Item = OutEvent> + '_ {
        let start = self.next;
        let end = start + self.labels[start..].partition_point(|(t, _)| *t < end_us);
        self.next = end;
        self.labels[start..end]
            .iter()
            .map(|(t, l)| OutEvent::new(*t, 'm', l.clone()))
    }
}

/// Writes one asciicast v3 event line (`[interval, "code", data]\n`), interval from the
/// previous event's time `prev_us` with six decimals.
pub(crate) fn v3_line(out: &mut Vec<u8>, prev_us: u64, ev: &OutEvent) {
    let dt = ev.t_us.saturating_sub(prev_us);
    let _ = write!(
        Utf8Vec(out),
        "[{}.{:06}, \"{}\", ",
        dt / 1_000_000,
        dt % 1_000_000,
        ev.code
    );
    json_string(out, &ev.data);
    out.extend_from_slice(b"]\n");
}

/// Appends `s` as a JSON string literal.
pub(crate) fn json_string(out: &mut Vec<u8>, s: &str) {
    // serde_json escapes control characters, quotes and backslashes; never fails for &str.
    let _ = serde_json::to_writer(&mut *out, s);
}

struct Utf8Vec<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Utf8Vec<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t_us: u64, code: EventCode, data: &str) -> Event {
        Event {
            t_us,
            code,
            data: data.into(),
        }
    }

    fn cmd(seq: u64, typed: &str) -> String {
        format!(r#"{{"cl":1,"k":"cmd","seq":{seq},"sh":5,"n":{seq},"typed":"{typed}"}}"#)
    }

    #[test]
    fn rewrites_markers() {
        let mask = Mask::default();
        let mut r = Rewriter::new(&mask, vec![], true);
        assert_eq!(
            r.push(ev(1, EventCode::Marker, &cmd(1, "ls"))),
            Some(OutEvent::new(1, 'm', "$ ls"))
        );
        let pause = r#"{"cl":1,"k":"pause","reason":"multiplexer"}"#;
        assert_eq!(r.push(ev(2, EventCode::Marker, pause)), None);
        assert_eq!(r.push(ev(3, EventCode::Marker, "not json")), None);
        assert_eq!(r.push(ev(4, EventCode::Other('q'), "?")), None);
        assert_eq!(r.push(ev(5, EventCode::Input, "a")), Some(OutEvent::new(5, 'i', "a")));
        assert_eq!(r.push(ev(6, EventCode::Exit, "0")), Some(OutEvent::new(6, 'x', "0")));
        let mut no_labels = Rewriter::new(&mask, vec![], false);
        assert_eq!(no_labels.push(ev(1, EventCode::Marker, &cmd(1, "ls"))), None);
    }

    #[test]
    fn masks_spans() {
        let mask = Mask::new(
            vec![
                Span {
                    from_us: 10,
                    to_us: 20,
                    by: "bob".into(),
                },
                Span {
                    from_us: 30,
                    to_us: u64::MAX,
                    by: "eve".into(),
                },
            ],
            HashSet::from([7]),
        );
        let mut r = Rewriter::new(&mask, vec![], true);
        let out: Vec<_> = [
            ev(5, EventCode::Output, "before"),
            ev(10, EventCode::Output, "secret"),
            ev(12, EventCode::Input, "pw"),
            ev(13, EventCode::Output, "more"),
            ev(14, EventCode::Resize, "100x30"),
            ev(15, EventCode::Marker, &cmd(3, "inside")),
            ev(20, EventCode::Output, "edge"),
            ev(21, EventCode::Output, "after"),
            ev(22, EventCode::Marker, &cmd(7, "forgotten seq")),
            ev(40, EventCode::Output, "open"),
            ev(41, EventCode::Output, "open2"),
        ]
        .into_iter()
        .filter_map(|e| r.push(e))
        .collect();
        assert_eq!(
            out,
            [
                OutEvent::new(5, 'o', "before"),
                OutEvent::new(10, 'o', placeholder("bob")),
                OutEvent::new(14, 'r', "100x30"),
                OutEvent::new(21, 'o', "after"),
                OutEvent::new(40, 'o', placeholder("eve")),
            ]
        );
        assert_eq!(r.shown(), [true, true]);
        // A stream that starts after the placeholder was shown does not repeat it.
        let mut later = Rewriter::new(&mask, vec![true], true);
        assert_eq!(later.push(ev(13, EventCode::Output, "more")), None);
        assert!(mask.key().contains("10-20-bob") && mask.key().ends_with("#7"));
        assert_eq!(mask.span_at(9), None);
        assert_eq!(mask.span_at(25), None);
    }

    #[test]
    fn resizes_are_clamped_or_dropped() {
        let mask = Mask::default();
        let mut r = Rewriter::new(&mask, vec![], true);
        assert_eq!(
            r.push(ev(1, EventCode::Resize, "65535x65535")),
            Some(OutEvent::new(1, 'r', "1000x500"))
        );
        assert_eq!(
            r.push(ev(2, EventCode::Resize, "80x24")),
            Some(OutEvent::new(2, 'r', "80x24"))
        );
        assert_eq!(r.push(ev(3, EventCode::Resize, "80x0")), None);
        assert_eq!(r.push(ev(4, EventCode::Resize, "wide")), None);
        let masked = Mask::new(
            vec![Span {
                from_us: 0,
                to_us: 10,
                by: "bob".into(),
            }],
            HashSet::new(),
        );
        let mut r = Rewriter::new(&masked, vec![], true);
        assert_eq!(
            r.push(ev(5, EventCode::Resize, "2000x30")),
            Some(OutEvent::new(5, 'r', "1000x30"))
        );
    }

    #[test]
    fn labels_merge_by_time() {
        let labels = vec![(5, "$ a".to_owned()), (10, "$ b".to_owned()), (15, "$ c".to_owned())];
        let mut m = LabelMerge::new(&labels, 6);
        assert_eq!(m.until(9).count(), 0);
        assert_eq!(m.until(10).map(|e| e.t_us).collect::<Vec<_>>(), [10]);
        assert_eq!(m.before(15).count(), 0);
        assert_eq!(m.before(16).map(|e| e.data).collect::<Vec<_>>(), ["$ c"]);
    }

    #[test]
    fn v3_lines() {
        let mut out = Vec::new();
        v3_line(&mut out, 1_000_000, &OutEvent::new(2_500_001, 'o', "a\"b\u{1b}[0m\n"));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[1.500001, \"o\", \"a\\\"b\\u001b[0m\\n\"]\n"
        );
    }
}
