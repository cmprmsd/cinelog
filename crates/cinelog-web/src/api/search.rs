//! `GET /api/search` and `GET /api/show` over the M2 query engine (docs/API.md §5.4, §5.5).

use cinelog_index::model::{self as im, Cursor, EntryRef, ExitFilter, SearchFilters, SearchQuery};

use super::db::{self, IndexDb};
use super::types::{
    Command, Entry, SearchHit, SearchIn, SearchParams, SearchResponse, ShowParams, SnippetPart, parse_tag_list,
    split_snippet,
};
use crate::error::{Result, WebError};
use crate::state::WebContext;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
const DEFAULT_SHOW_BYTES: u32 = 64 << 10;
const MAX_SHOW_BYTES: u32 = 1 << 20;

fn finite(v: Option<f64>, name: &str) -> Result<Option<f64>> {
    match v {
        Some(x) if !x.is_finite() => Err(WebError::BadRequest(format!("{name} must be a finite number"))),
        v => Ok(v),
    }
}

/// Checks `limit` against `1..=max`, `default` when absent.
pub(crate) fn limit(v: Option<u32>, default: u32, max: u32) -> Result<usize> {
    match v.unwrap_or(default) {
        n if (1..=max).contains(&n) => Ok(n as usize),
        _ => Err(WebError::BadRequest(format!("limit must be 1…{max}"))),
    }
}

/// The index query for `p`.
pub(crate) fn query(p: &SearchParams) -> Result<SearchQuery> {
    let exit = p
        .exit
        .as_deref()
        .map(|e| e.trim().parse::<ExitFilter>())
        .transpose()
        .map_err(|_| WebError::BadRequest("exit must be a number, ok or fail".into()))?;
    let cursor = p
        .cursor
        .as_deref()
        .map(str::parse::<Cursor>)
        .transpose()
        .map_err(|_| WebError::BadRequest("invalid cursor".into()))?;
    Ok(SearchQuery {
        term: p.q.clone().unwrap_or_default(),
        target: p.target.unwrap_or(SearchIn::Cmd).into(),
        case_sensitive: p.case.unwrap_or(false),
        filters: SearchFilters {
            user: p.user.clone(),
            host: p.host.clone(),
            since: finite(p.from, "from")?,
            until: finite(p.to, "to")?,
            exit,
            cwd: p.cwd.clone(),
            pane: p.pane.clone(),
            session: p.session.as_deref().map(db::parse_uuid).transpose()?,
            include_hidden: p.hidden.unwrap_or(false),
            tags: p
                .tag
                .as_deref()
                .map(parse_tag_list)
                .transpose()
                .map_err(|e| WebError::BadRequest(e.to_string()))?
                .unwrap_or_default(),
        },
        limit: limit(p.limit, DEFAULT_LIMIT, MAX_LIMIT)?,
        cursor,
    })
}

pub(crate) fn search(ctx: &WebContext, p: &SearchParams) -> Result<SearchResponse> {
    let q = query(p)?;
    let page = db::searcher(ctx)?.search(&q)?;
    let uuids: Vec<&str> = page.hits.iter().map(|h| h.session_uuid.as_str()).collect();
    let context = IndexDb::open(ctx)?.hit_context(&uuids)?;
    let hits = page
        .hits
        .iter()
        .map(|h| {
            let (pane, tmux_session, legacy) = context.get(&h.session_uuid).cloned().unwrap_or_default();
            hit(h, &q, pane, tmux_session, legacy)
        })
        .collect();
    Ok(SearchResponse {
        hits,
        next: page.next.map(|c| c.to_string()),
        partial: page.partial,
        capped: page.capped,
    })
}

fn hit(
    h: &im::SearchHit,
    q: &SearchQuery,
    pane: Option<String>,
    tmux_session: Option<String>,
    legacy: bool,
) -> SearchHit {
    let (id, mut snippet) = match h.kind {
        im::HitKind::Command => (
            format!("c:{}", h.id),
            mark(h.typed.as_deref().unwrap_or_default(), &q.term, q.case_sensitive),
        ),
        im::HitKind::Output => (
            format!("o:{}", h.id),
            split_snippet(h.snippet.as_deref().unwrap_or_default()),
        ),
    };
    if snippet.is_empty() {
        snippet.push(SnippetPart {
            text: h.typed.clone().unwrap_or_default(),
            hit: false,
        });
    }
    SearchHit {
        id,
        kind: h.kind.into(),
        session: h.session_uuid.clone(),
        cmd_seq: h.cmd_seq,
        t: h.t,
        ts: h.ts,
        user: h.user.clone(),
        host: h.host.clone(),
        pane,
        tmux_session,
        exit: h.exit,
        cwd: h.cwd.clone(),
        typed: h.typed.clone(),
        snippet,
        chunk_kind: h.chunk_kind.map(Into::into),
        approx: h.approx,
        confidence: h.confidence,
        legacy,
        // The searcher reads them from meta.db with the page.
        tags: h.tags.clone(),
    }
}

/// The index's 1:1 case folding (FTS5 `case_sensitive=0`): a character whose lowercase form
/// has several characters stays as it is, so folded text keeps the character count.
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// `text` split at every non-overlapping match of `term` (left to right), with the same
/// folding as the search; adjacent parts of one kind are merged. An empty term marks
/// nothing.
pub(crate) fn mark(text: &str, term: &str, case_sensitive: bool) -> Vec<SnippetPart> {
    let norm = |c: char| if case_sensitive { c } else { fold_char(c) };
    let needle: Vec<char> = term.chars().map(norm).collect();
    let chars: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, norm(c))).collect();
    let mut parts: Vec<SnippetPart> = Vec::new();
    let mut push = |s: &str, hit: bool| match parts.last_mut() {
        _ if s.is_empty() => {}
        Some(last) if last.hit == hit => last.text.push_str(s),
        _ => parts.push(SnippetPart {
            text: s.to_owned(),
            hit,
        }),
    };
    let byte_at = |i: usize| chars.get(i).map_or(text.len(), |&(b, _)| b);
    let (mut i, mut plain_from) = (0, 0);
    while !needle.is_empty() && i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()]
            .iter()
            .map(|&(_, c)| c)
            .eq(needle.iter().copied())
        {
            push(&text[byte_at(plain_from)..byte_at(i)], false);
            push(&text[byte_at(i)..byte_at(i + needle.len())], true);
            i += needle.len();
            plain_from = i;
        } else {
            i += 1;
        }
    }
    push(&text[byte_at(plain_from)..], false);
    parts
}

pub(crate) fn show(ctx: &WebContext, p: &ShowParams) -> Result<Entry> {
    let entry: EntryRef = p
        .entry
        .trim()
        .parse()
        .map_err(|_| WebError::BadRequest("entry must be <uuid>, <uuid>:<seq> or <uuid>@<t>".into()))?;
    let max = match p.max_bytes.unwrap_or(DEFAULT_SHOW_BYTES) {
        n if (1..=MAX_SHOW_BYTES).contains(&n) => n as usize,
        _ => return Err(WebError::BadRequest(format!("max_bytes must be 1…{MAX_SHOW_BYTES}"))),
    };
    let detail = db::searcher(ctx)?.show(&entry, max)?.ok_or(WebError::NotFound)?;
    let session = IndexDb::open(ctx)?
        .session(entry.uuid())?
        .ok_or(WebError::NotFound)?
        .session;
    Ok(Entry {
        session,
        command: detail.command.as_ref().map(Command::from),
        output: detail.output,
        output_truncated: detail.output_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(v: &[SnippetPart]) -> Vec<(&str, bool)> {
        v.iter().map(|p| (p.text.as_str(), p.hit)).collect()
    }

    #[test]
    fn marks_every_match() {
        assert_eq!(
            parts(&mark("ls -la; LS", "ls", false)),
            [("ls", true), (" -la; ", false), ("LS", true)]
        );
        assert_eq!(
            parts(&mark("ls -la; LS", "ls", true)),
            [("ls", true), (" -la; LS", false)]
        );
        assert_eq!(parts(&mark("aaaaa", "aa", false)), [("aaaa", true), ("a", false)]);
        assert_eq!(
            parts(&mark("echo ÄRGER", "ärger", false)),
            [("echo ", false), ("ÄRGER", true)]
        );
        assert_eq!(parts(&mark("plain", "", false)), [("plain", false)]);
        assert_eq!(parts(&mark("plain", "zzz", false)), [("plain", false)]);
        assert!(mark("", "x", false).is_empty());
        // Markup in commands stays text.
        assert_eq!(
            parts(&mark("<b>x</b>", "x", false)),
            [("<b>", false), ("x", true), ("</b>", false)]
        );
    }

    #[test]
    fn params_become_a_query() {
        let p = SearchParams {
            q: Some("docker".into()),
            target: Some(SearchIn::Both),
            case: Some(true),
            exit: Some("fail".into()),
            session: Some("0193A1B2-0000-7000-8000-000000000001".into()),
            limit: Some(10),
            ..SearchParams::default()
        };
        let q = query(&p).unwrap();
        assert_eq!(q.target, im::SearchTarget::All);
        assert!(q.case_sensitive);
        assert_eq!(q.filters.exit, Some(ExitFilter::Failure));
        assert_eq!(
            q.filters.session.as_deref(),
            Some("0193a1b2-0000-7000-8000-000000000001")
        );
        assert_eq!(q.limit, 10);
        assert_eq!(query(&SearchParams::default()).unwrap().limit, 50);
        for bad in [
            SearchParams {
                limit: Some(0),
                ..SearchParams::default()
            },
            SearchParams {
                limit: Some(501),
                ..SearchParams::default()
            },
            SearchParams {
                exit: Some("maybe".into()),
                ..SearchParams::default()
            },
            SearchParams {
                cursor: Some("garbage".into()),
                ..SearchParams::default()
            },
            SearchParams {
                session: Some("../x".into()),
                ..SearchParams::default()
            },
            SearchParams {
                from: Some(f64::INFINITY),
                ..SearchParams::default()
            },
        ] {
            assert!(matches!(query(&bad), Err(WebError::BadRequest(_))), "{bad:?}");
        }
    }
}
