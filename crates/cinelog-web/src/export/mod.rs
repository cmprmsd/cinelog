//! Export (work package M3-B; plan §7.5, §7.6; docs/API.md "Export").
//!
//! Provides, with exactly this signature, [`routes`]: mounted under `/api` behind
//! `auth::require_session`: `GET /sessions/{uuid}/export` (`ExportParams`), streamed with
//! `Content-Disposition: attachment`:
//! - `cast`: asciicast v3 slice: header, a t=0 `o` event with the avt dump at `from`, then
//!   the events up to `to` (relative times; same label rewrite and masking as playback);
//! - `txt`: the rendered rows of the range as UTF-8 text;
//! - `html`: one self-contained document: every character escaped, SGR colours and
//!   attributes as inline-styled spans (whitelist), no script, its own CSP meta tag.
//!
//! A slice starts at the playback segment at or before `from` (large casts) or at the
//! beginning (small ones), so memory is one avt screen plus buffers whatever the cast size.

pub(crate) mod render;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::api::db::{Forgets, IndexDb, blocking, parse_uuid};
use crate::api::extract::{ApiPath, ApiQuery};
use crate::api::sessions::visible_session;
use crate::api::types::{ExportFormat, ExportParams, PlayMode};
use crate::error::{Result, WebError};
use crate::play::body::{Sink, streamed};
use crate::play::plan::{apply, terminal};
use crate::play::session::{Loaded, mode_for};
use crate::play::source::{Events, StartPoint, parse_size, secs_to_us, us_to_secs};
use crate::play::stream::{LabelMerge, OutEvent, Rewriter, v3_line};
use crate::play::{CAST_CONTENT_TYPE, cast::header_line};
use crate::state::AppState;

use self::render::{HTML_TAIL, LineWriter, Renderer, html_head};

pub fn routes() -> Router<AppState> {
    Router::new().route("/sessions/{uuid}/export", get(export))
}

/// The exported range in µs of session time, inclusive; `to = None` runs to the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub from_us: u64,
    pub to_us: Option<u64>,
}

impl Range {
    fn contains(&self, t: u64) -> bool {
        t >= self.from_us && self.to_us.is_none_or(|to| t <= to)
    }

    fn after(&self, t: u64) -> bool {
        self.to_us.is_some_and(|to| t > to)
    }
}

fn seconds(v: Option<f64>, name: &str) -> Result<Option<f64>> {
    match v {
        Some(x) if !x.is_finite() || x < 0.0 => Err(WebError::BadRequest(format!(
            "{name} must be a non-negative number of seconds"
        ))),
        v => Ok(v),
    }
}

/// The range `p` selects in session `uuid`.
fn range(st: &AppState, uuid: &str, p: &ExportParams) -> Result<Range> {
    let (from, to) = (seconds(p.from, "from")?, seconds(p.to, "to")?);
    match p.cmd {
        Some(_) if from.is_some() || to.is_some() => {
            Err(WebError::BadRequest("give either cmd or from/to, not both".into()))
        }
        Some(seq) => {
            let db = IndexDb::open(&st.ctx)?;
            let forgets = Forgets::load(&st.ctx)?;
            let row = visible_session(&db, &forgets, uuid)?;
            let c = db.command(row.id, seq)?.ok_or(WebError::NotFound)?;
            if forgets.command(uuid, &c) {
                return Err(WebError::NotFound);
            }
            Ok(Range {
                from_us: secs_to_us(c.t_start),
                to_us: c.t_end.map(secs_to_us),
            })
        }
        None => {
            let from_us = secs_to_us(from.unwrap_or(0.0));
            let to_us = to.map(secs_to_us);
            if to_us.is_some_and(|t| t < from_us) {
                return Err(WebError::BadRequest("from must not be after to".into()));
            }
            Ok(Range { from_us, to_us })
        }
    }
}

/// Seconds for a file name: `12`, `12.5`, `0.001`.
fn name_secs(us: u64) -> String {
    let s = format!("{:.3}", us_to_secs(us));
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

pub(crate) fn file_name(uuid: &str, r: Range, format: ExportFormat) -> String {
    let ext = match format {
        ExportFormat::Cast => "cast",
        ExportFormat::Txt => "txt",
        ExportFormat::Html => "html",
    };
    let to = r.to_us.map_or_else(|| "end".to_owned(), name_secs);
    let short: String = uuid.chars().filter(char::is_ascii_hexdigit).take(8).collect();
    format!("cinelog-{short}-{}-{to}.{ext}", name_secs(r.from_us))
}

/// Where reading starts for a range beginning at `from_us`: the terminal there as far as a
/// segment knows it.
struct Origin {
    at: StartPoint,
    start_us: u64,
    size: (u16, u16),
    init: String,
    shown: Vec<bool>,
}

fn origin(st: &AppState, s: &Loaded, from_us: u64) -> Result<Origin> {
    if mode_for(&st.ctx, s.files.total_bytes()) == PlayMode::Segmented && from_us > 0 {
        let plan = st.play.plan(&st.ctx, s.uuid())?;
        let seg = plan.segment_at(from_us);
        // A plan made before the latest forget would show its spans: it is rebuilt then.
        if plan.mask == s.mask {
            return Ok(Origin {
                at: seg.at,
                start_us: seg.start_us,
                size: seg.size,
                init: seg.init.clone(),
                shown: seg.shown.clone(),
            });
        }
    }
    let h = s.files.header()?;
    Ok(Origin {
        at: StartPoint::ORIGIN,
        start_us: 0,
        size: h.term.size(),
        init: String::new(),
        shown: Vec::new(),
    })
}

/// The masked, relabelled events of the session from `o` on, until `r` ends or `each`
/// returns `false`.
fn for_each_event(s: &Loaded, o: &Origin, r: Range, mut each: impl FnMut(OutEvent) -> Result<bool>) -> Result<()> {
    let mut events = Events::open(&s.files, o.at)?;
    let from_index = s.legacy();
    let mut rw = Rewriter::new(&s.mask, o.shown.clone(), !from_index);
    let none = Vec::new();
    let mut labels = LabelMerge::new(if from_index { &s.labels } else { &none }, o.start_us);
    while let Some(next) = events.next_event() {
        let (_, ev) = next?;
        if r.after(ev.t_us) {
            break;
        }
        let Some(e) = rw.push(ev) else { continue };
        for l in labels.until(e.t_us).collect::<Vec<_>>() {
            if !each(l)? {
                return Ok(());
            }
        }
        if !each(e)? {
            return Ok(());
        }
    }
    if r.to_us.is_some() {
        for l in labels.before(r.to_us.map_or(u64::MAX, |t| t + 1)).collect::<Vec<_>>() {
            if !each(l)? {
                break;
            }
        }
    }
    Ok(())
}

/// The v3 header timestamp of a slice starting `from_us` into a session started at
/// `started_at` (epoch seconds, µs precision): the start of the slice floored to whole
/// seconds, as the recorder floors `start_epoch_us` for its own header, so event `t` of the
/// slice happened at most 1 s after `timestamp + t`, never before it.
fn slice_timestamp(started_at: f64, from_us: u64) -> i64 {
    let start_us = (started_at * 1e6).round() as i64;
    start_us.saturating_add(from_us as i64).div_euclid(1_000_000)
}

/// A standalone v3 cast of the range: the terminal at `from` as one output event at t=0.
fn write_cast(s: &Loaded, o: Origin, r: Range, sink: &mut Sink) -> Result<()> {
    let header = s.files.header()?;
    let mut size = o.size;
    let mut vt = terminal(size, &o.init);
    let mut started = false;
    let mut prev = r.from_us;
    let start = |sink: &mut Sink, vt: &avt::Terminal, size: (u16, u16)| -> Result<()> {
        let mut h = header.clone();
        h.term.cols = size.0;
        h.term.rows = size.1;
        h.timestamp = h
            .timestamp
            .map(|_| slice_timestamp(s.row.session.started_at, r.from_us));
        sink.write(&header_line(&h))?;
        v3_line(sink.buf(), 0, &OutEvent::new(0, 'o', vt.dump()));
        sink.maybe_flush()
    };
    for_each_event(s, &o, r, |e| {
        if !r.contains(e.t_us) {
            if e.code != 'm' {
                apply(&mut vt, &mut size, &e);
            }
            return Ok(true);
        }
        if !started {
            start(sink, &vt, size)?;
            started = true;
        }
        let rel = OutEvent::new(e.t_us - r.from_us, e.code, e.data);
        v3_line(sink.buf(), prev - r.from_us, &rel);
        prev = prev.max(e.t_us);
        sink.maybe_flush()?;
        Ok(true)
    })?;
    if !started {
        start(sink, &vt, size)?;
    }
    Ok(())
}

/// The rows rendered from the range's output, as text or HTML.
fn write_rows(s: &Loaded, o: Origin, r: Range, html: bool, sink: &mut Sink) -> Result<()> {
    if html {
        let title = format!(
            "cinelog {} {}–{}",
            s.uuid(),
            name_secs(r.from_us),
            r.to_us.map_or_else(|| "end".into(), name_secs)
        );
        sink.write(html_head(&title).as_bytes())?;
    }
    let mut size = o.size;
    let mut renderer: Option<Renderer> = None;
    let mut lines = LineWriter::new(html);
    for_each_event(s, &o, r, |e| {
        if !r.contains(e.t_us) {
            // Before the range only the terminal size matters: rows start empty at `from`.
            if e.code == 'r'
                && let Some(sz) = parse_size(&e.data)
            {
                size = sz;
            }
            return Ok(true);
        }
        let rd = renderer.get_or_insert_with(|| Renderer::new(size));
        match e.code {
            'o' => rd.output(&e.data),
            'r' => {
                if let Some((c, rows)) = parse_size(&e.data) {
                    rd.resize(c, rows);
                }
            }
            _ => return Ok(true),
        }
        lines.rows(rd.take(), sink.buf());
        sink.maybe_flush()?;
        Ok(true)
    })?;
    if let Some(rd) = renderer {
        lines.rows(rd.finish(), sink.buf());
    }
    lines.finish(sink.buf());
    if html {
        sink.write(HTML_TAIL.as_bytes())?;
    }
    Ok(())
}

async fn export(
    State(st): State<AppState>,
    ApiPath(uuid): ApiPath<String>,
    ApiQuery(p): ApiQuery<ExportParams>,
) -> Result<Response> {
    let uuid = parse_uuid(&uuid)?;
    let st2 = st.clone();
    let format = p.format;
    let (s, r, o) = blocking(move || {
        let r = range(&st2, &uuid, &p)?;
        let s = Loaded::load(&st2.ctx, &st2.play.markers, &uuid)?;
        let o = origin(&st2, &s, r.from_us)?;
        Ok((s, r, o))
    })
    .await?;
    let name = file_name(s.uuid(), r, format);
    let content_type = match format {
        ExportFormat::Cast => CAST_CONTENT_TYPE,
        ExportFormat::Txt => "text/plain; charset=utf-8",
        ExportFormat::Html => "text/html; charset=utf-8",
    };
    let body = streamed("export", move |sink| match format {
        ExportFormat::Cast => write_cast(&s, o, r, sink),
        ExportFormat::Txt => write_rows(&s, o, r, false, sink),
        ExportFormat::Html => write_rows(&s, o, r, true, sink),
    });
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    let disposition = format!("attachment; filename=\"{name}\"");
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(|e| WebError::Internal(e.to_string()))?,
    );
    Ok(resp)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn file_names() {
        let r = Range {
            from_us: 12_500_000,
            to_us: Some(30_000_000),
        };
        assert_eq!(
            file_name("0193a1b2-0000-7000-8000-000000000001", r, ExportFormat::Txt),
            "cinelog-0193a1b2-12.5-30.txt"
        );
        let open = Range {
            from_us: 0,
            to_us: None,
        };
        assert_eq!(
            file_name("0193a1b2-x", open, ExportFormat::Cast),
            "cinelog-0193a1b2-0-end.cast"
        );
        assert_eq!(name_secs(1_000), "0.001");
    }

    #[test]
    fn ranges() {
        let r = Range {
            from_us: 5,
            to_us: Some(10),
        };
        assert!(r.contains(5) && r.contains(10) && !r.contains(11) && !r.contains(4));
        assert!(r.after(11) && !r.after(10));
    }

    #[test]
    fn slice_timestamps() {
        // 0.7 s + 1.6 s past a whole second is 2.3 s past it: floor(start) + floor(from)
        // would say 1 s.
        assert_eq!(slice_timestamp(1_731_070_000.7, 1_600_000), 1_731_070_002);
        assert_eq!(slice_timestamp(1_731_070_000.7, 0), 1_731_070_000);
        assert_eq!(slice_timestamp(1_731_070_000.7, 300_000), 1_731_070_001);
        assert_eq!(slice_timestamp(1_731_070_000.0, 999_999), 1_731_070_000);
        assert_eq!(slice_timestamp(1_731_070_000.123_456, 876_544), 1_731_070_001);
    }
}
