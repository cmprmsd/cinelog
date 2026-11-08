//! `GET /api/sessions/{uuid}/cast` (docs/API.md §6.1): one asciicast v3 document over all
//! parts, labels rewritten, forgotten spans masked.

use super::body::Sink;
use super::session::Loaded;
use super::source::{Events, StartPoint};
use super::stream::{LabelMerge, OutEvent, Rewriter, v3_line};
use crate::error::Result;

/// The header served for a session: part 1's, as v3 (`version` first), its size clamped.
pub(crate) fn header_line(header: &cinelog_cast::Header) -> Vec<u8> {
    let mut h = header.clone();
    h.version = cinelog_cast::CAST_VERSION;
    (h.term.cols, h.term.rows) = super::source::clamp_size((h.term.cols, h.term.rows));
    let mut out = serde_json::to_vec(&h).unwrap_or_else(|_| unreachable!("headers serialise"));
    out.push(b'\n');
    out
}

/// Writes the whole session. cinelog casts carry their chapters as `cmd` markers; legacy
/// casts have none, so their chapters come from the index.
pub(crate) fn write_cast(s: &Loaded, sink: &mut Sink) -> Result<()> {
    let mut events = Events::open(&s.files, StartPoint::ORIGIN)?;
    sink.write(&header_line(events.header()))?;
    let from_index = s.legacy();
    let mut rw = Rewriter::new(&s.mask, Vec::new(), !from_index);
    let no_labels = Vec::new();
    let mut labels = LabelMerge::new(if from_index { &s.labels } else { &no_labels }, 0);
    let mut prev = 0u64;
    let emit = |sink: &mut Sink, e: &OutEvent, prev: &mut u64| -> Result<()> {
        v3_line(sink.buf(), *prev, e);
        *prev = (*prev).max(e.t_us);
        sink.maybe_flush()
    };
    while let Some(next) = events.next_event() {
        let (_, ev) = next?;
        if let Some(e) = rw.push(ev) {
            for l in labels.until(e.t_us).collect::<Vec<_>>() {
                emit(sink, &l, &mut prev)?;
            }
            emit(sink, &e, &mut prev)?;
        }
    }
    for l in labels.before(u64::MAX).collect::<Vec<_>>() {
        emit(sink, &l, &mut prev)?;
    }
    Ok(())
}
