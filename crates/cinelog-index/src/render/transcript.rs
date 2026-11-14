//! [`AvtTranscript`]: avt 0.20 with scrollback 0 and an observer that turns rows leaving
//! the primary screen into owner-attributed chunks (plan §6.4).
//!
//! Rows are numbered absolutely: `base` counts rows that left the terminal, so the line at
//! index `j` of `primary_screen().all_lines()` is row `base + j`, and `meta[j]` is its
//! bookkeeping. Between feed calls the terminal never retains history (limit 0), so `base`
//! is also the number of the top visible row.
//!
//! Every row is either pending (owned by the current span) or done (emitted by a closed
//! span, with the hash of the text it had then). A boundary emits the pending rows above
//! the cursor; eviction emits pending rows and skips done ones. A done row becomes pending
//! again when it is erased to blank or printed over with different text, so output after a
//! clear that homes the cursor without scrolling (`\E[H\E[J`) is not lost. A resize remaps
//! the states by logical line, since reflow changes the rows of every wrapped line.
//!
//! Row times are first-write times (`dirty()` of a non-blank row at or above the cursor);
//! scroll regions and IL/DL shift rows without telling the observer, which can misattribute
//! times near such operations (accepted, §14 risk 8).

use std::collections::{HashSet, VecDeque};
use std::ops::Range;

use avt::parser::{DecMode, Function};
use avt::{Line, Observation, Observer, Operation, ScreenKind, Terminal};
use serde::{Deserialize, Serialize};

use cinelog_cast::sane_size;

use super::chunker::{Chunker, wanted};
use super::exact::{F64, XStamp};
use crate::api::Transcript;
use crate::error::{IndexError, Result};
use crate::model::{Chunk, ChunkKind, OutputStats, Owner, RenderOptions, Stamp, TranscriptState};

/// Logical lines kept for [`Transcript::lines_above_cursor`].
const RECENT_LINES: usize = 16;
/// Longest line kept there (bytes).
const RECENT_LINE_BYTES: usize = 2048;
/// Alt-screen line hashes remembered per span for de-duplication.
const ALT_SEEN: usize = 4096;

/// Hashes of alt lines already emitted in the current span, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "VecDeque<u64>", into = "VecDeque<u64>")]
struct Seen {
    order: VecDeque<u64>,
    set: HashSet<u64>,
}

impl From<VecDeque<u64>> for Seen {
    fn from(order: VecDeque<u64>) -> Self {
        Self {
            set: order.iter().copied().collect(),
            order,
        }
    }
}

impl From<Seen> for VecDeque<u64> {
    fn from(s: Seen) -> Self {
        s.order
    }
}

impl Seen {
    fn insert(&mut self, h: u64) -> bool {
        if !self.set.insert(h) {
            return false;
        }
        self.order.push_back(h);
        if self.order.len() > ALT_SEEN
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }

    fn clear(&mut self) {
        self.order.clear();
        self.set.clear();
    }
}

/// FNV-1a: a hash that stays stable across builds (it is persisted in checkpoints).
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Alt {
    active: bool,
    last_snapshot: F64,
    /// Last write time per alt row (full-screen programs redraw constantly, so the last
    /// write is the moment that shows the snapshotted content).
    row_t: Vec<Option<XStamp>>,
    seen: Seen,
}

/// Logical lines of the rows that scrolled off, for `lines_above_cursor`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Recent {
    lines: VecDeque<String>,
    /// A wrapped row whose continuation has not scrolled off yet.
    partial: String,
}

impl Recent {
    fn push(&mut self, line: &Line) {
        if self.partial.len() < RECENT_LINE_BYTES {
            self.partial.extend(line.chars());
        }
        if !line.is_wrapped() {
            let mut text = std::mem::take(&mut self.partial);
            truncate_at_char(&mut text, RECENT_LINE_BYTES);
            text.truncate(text.trim_end().len());
            self.lines.push_back(text);
            if self.lines.len() > RECENT_LINES {
                self.lines.pop_front();
            }
        }
    }
}

fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() > max {
        let mut i = max;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
}

/// Bookkeeping of one primary row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct Row {
    /// First-write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t: Option<XStamp>,
    /// Hash of the text the row had when it was emitted; `None` while it is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    done: Option<u64>,
}

fn row_hash(line: &Line) -> u64 {
    fnv1a(line.text().trim_end())
}

/// Emission state of one logical line before a reflow.
#[derive(Debug, Clone, Copy)]
struct LineState {
    /// Every row was emitted.
    done: bool,
    /// Cells of the leading emitted rows.
    prefix: usize,
    /// First-write time of the pending part.
    t: Option<XStamp>,
}

/// What an operation did to the rows it dirtied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Write {
    Print,
    Erase,
    Other,
}

fn write_kind(op: &Operation<'_>) -> Write {
    match op {
        Operation::Function(Function::Print(_) | Function::Rep(_) | Function::Decaln) => Write::Print,
        Operation::Function(Function::Ed(_) | Function::El(_) | Function::Ech(_) | Function::Dch(_)) => Write::Erase,
        _ => Write::Other,
    }
}

/// A logical line of the current span that has started but not ended (soft wrap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Pending {
    at: XStamp,
    text: String,
}

/// Everything besides the avt terminal; serialized into [`TranscriptState::state`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    cols: u16,
    rows: u16,
    base: u64,
    /// Rows `base..`; rows past the end are pending and unwritten.
    meta: VecDeque<Row>,
    owner: Owner,
    pending: Option<Pending>,
    chunker: Chunker,
    alt: Alt,
    recent: Recent,
    now: XStamp,
}

/// The avt-backed [`Transcript`] (see the module docs).
pub(crate) struct AvtTranscript {
    term: Terminal,
    opts: RenderOptions,
    st: State,
    /// `dirty()` ranges of the running action; resolved in `after()`.
    dirty: Vec<Range<usize>>,
}

impl AvtTranscript {
    /// A terminal of the cast header's size: `cols x rows`, or [`cinelog_cast::FALLBACK_SIZE`]
    /// when that is not a [`sane_size`].
    pub(crate) fn new(cols: u16, rows: u16, opts: RenderOptions) -> Result<Self> {
        let (cols, rows) = sane_size(cols, rows).unwrap_or(cinelog_cast::FALLBACK_SIZE);
        Ok(Self {
            term: new_terminal(cols, rows)?,
            opts,
            st: State {
                cols,
                rows,
                base: 0,
                meta: VecDeque::new(),
                owner: Owner::Gap,
                pending: None,
                chunker: Chunker::default(),
                alt: Alt::default(),
                recent: Recent::default(),
                now: XStamp::default(),
            },
            dirty: Vec::new(),
        })
    }

    pub(crate) fn restore(state: &TranscriptState, opts: RenderOptions) -> Result<Self> {
        let st: State = serde_json::from_str(&state.state)?;
        if sane_size(st.cols, st.rows).is_none() {
            return Err(IndexError::Invalid(
                "transcript state: terminal size out of range".into(),
            ));
        }
        if st.base != state.evicted {
            return Err(IndexError::Invalid("transcript state: evicted counter mismatch".into()));
        }
        let mut term = new_terminal(st.cols, st.rows)?;
        term.feed(state.vt_dump.as_str());
        Ok(Self {
            term,
            opts,
            st,
            dirty: Vec::new(),
        })
    }

    /// avt `dump()` of the current terminal (playback segments).
    pub(crate) fn vt_dump(&self) -> String {
        self.term.dump()
    }

    pub(crate) fn size(&self) -> (u16, u16) {
        (self.st.cols, self.st.rows)
    }

    /// Ends the current span at absolute row `cut` (exclusive) and flushes it.
    fn close_span(&mut self, cut: u64) {
        let opts = self.opts;
        if self.term.active_screen_kind() == ScreenKind::Alternate {
            self.st.snapshot_alt(&opts, &self.term);
        }
        self.st.commit_visible(&opts, &self.term, cut);
        self.st.end_pending(&opts);
        self.st.chunker.close();
    }

    /// Drops the output accounting of a closed command (after `output_stats` was read).
    pub(crate) fn forget_owner(&mut self, owner: Owner) {
        self.st.chunker.forget(owner);
    }

    /// Absolute row after the last non-blank primary row.
    fn primary_end(&self) -> u64 {
        self.st.base + occupied(self.term.primary_screen().visible_lines()) as u64
    }
}

fn new_terminal(cols: u16, rows: u16) -> Result<Terminal> {
    Terminal::with_scrollback(usize::from(cols), usize::from(rows), Some(0))
        .map_err(|e| IndexError::Invalid(format!("terminal size: {e}")))
}

fn is_blank(line: &Line) -> bool {
    line.chars().all(|c| c == ' ')
}

/// Number of rows through the last non-blank one.
fn occupied<'a>(rows: impl Iterator<Item = &'a Line>) -> usize {
    rows.enumerate()
        .filter(|(_, l)| !is_blank(l))
        .last()
        .map_or(0, |(i, _)| i + 1)
}

/// Joins soft-wrapped rows into trimmed logical lines with the index of their first row.
fn logical_lines<'a>(rows: impl Iterator<Item = &'a Line>, mut prefix: String) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut first = 0;
    for (i, line) in rows.enumerate() {
        if prefix.is_empty() {
            first = i;
        }
        prefix.extend(line.chars());
        if !line.is_wrapped() {
            let mut text = std::mem::take(&mut prefix);
            text.truncate(text.trim_end().len());
            out.push((first, text));
        }
    }
    if !prefix.is_empty() {
        prefix.truncate(prefix.trim_end().len());
        out.push((first, prefix));
    }
    out
}

impl State {
    fn now(&self) -> Stamp {
        self.now.into()
    }

    /// One row of the current span reaches the transcript.
    fn push_row(&mut self, opts: &RenderOptions, line: &Line, at: Stamp) {
        let p = self.pending.get_or_insert_with(|| Pending {
            at: at.into(),
            text: String::new(),
        });
        p.text.extend(line.chars());
        // A pathological unwrapped stream (minified JS, base64) is cut into chunk-sized lines.
        if !line.is_wrapped() || p.text.len() >= opts.chunk_bytes {
            self.end_pending(opts);
        }
    }

    fn end_pending(&mut self, opts: &RenderOptions) {
        if let Some(p) = self.pending.take() {
            self.chunker.line(opts, self.owner, p.text.trim_end(), p.at.into());
        }
    }

    fn row(&self, abs: u64) -> Row {
        abs.checked_sub(self.base)
            .and_then(|i| self.meta.get(usize::try_from(i).ok()?).copied())
            .unwrap_or_default()
    }

    fn row_mut(&mut self, idx: usize) -> &mut Row {
        if self.meta.len() <= idx {
            self.meta.resize(idx + 1, Row::default());
        }
        &mut self.meta[idx]
    }

    fn row_time(&self, abs: u64) -> Stamp {
        self.row(abs).t.map_or_else(|| self.now(), Stamp::from)
    }

    /// Emits row `base + idx` for the current span unless it is done already.
    fn emit(&mut self, opts: &RenderOptions, idx: usize, line: &Line) {
        let now = self.now();
        let r = self.row_mut(idx);
        if r.done.is_some() {
            self.end_pending(opts);
            return;
        }
        r.done = Some(row_hash(line));
        let at = r.t.map_or(now, Stamp::from);
        self.push_row(opts, line, at);
    }

    /// Emits the pending visible primary rows above absolute row `cut`.
    fn commit_visible(&mut self, opts: &RenderOptions, term: &Terminal, cut: u64) {
        let history = history_len(term);
        let top = self.base + history as u64;
        let n = usize::try_from(cut.saturating_sub(top)).unwrap_or(usize::MAX);
        for (i, line) in term.primary_screen().visible_lines().take(n).enumerate() {
            self.emit(opts, history + i, line);
        }
    }

    /// A row left the terminal (avt eviction, or discarded by a hard reset).
    fn evict(&mut self, opts: &RenderOptions, line: &Line) {
        self.base += 1;
        let r = self.meta.pop_front().unwrap_or_default();
        self.recent.push(line);
        if r.done.is_some() {
            self.end_pending(opts);
        } else {
            let at = r.t.map_or_else(|| self.now(), Stamp::from);
            self.push_row(opts, line, at);
        }
    }

    /// RIS discards every primary row without eviction callbacks: treat them as evicted.
    fn hard_reset(&mut self, opts: &RenderOptions, term: &Terminal) {
        if term.active_screen_kind() == ScreenKind::Alternate {
            self.snapshot_alt(opts, term);
        }
        let screen = term.primary_screen();
        let last = occupied(screen.all_lines());
        let total = screen.all_lines().size_hint().0;
        for line in screen.all_lines().take(last) {
            self.evict(opts, line);
        }
        self.base += (total - last) as u64;
        self.meta.clear();
        self.end_pending(opts);
        self.recent.partial.clear();
    }

    /// Emission state per logical line of the primary screen (before a reflow).
    fn line_states(&self, term: &Terminal) -> Vec<LineState> {
        let cols = usize::from(self.cols);
        let mut out = Vec::new();
        let mut cur: Option<LineState> = None;
        for (i, line) in term.primary_screen().all_lines().enumerate() {
            let r = self.meta.get(i).copied().unwrap_or_default();
            let s = cur.get_or_insert(LineState {
                done: true,
                prefix: 0,
                t: None,
            });
            match r.done {
                Some(_) if s.done => s.prefix += cols,
                Some(_) => {}
                None => {
                    s.done = false;
                    s.t = s.t.or(r.t);
                }
            }
            if !line.is_wrapped() {
                out.extend(cur.take());
            }
        }
        out.extend(cur);
        out
    }

    /// Re-derives the row states after a reflow from the states of the logical lines.
    fn remap(&mut self, term: &Terminal, states: &[LineState]) {
        let cols = term.size().0;
        let (mut li, mut k) = (0, 0);
        let mut meta = VecDeque::new();
        for line in term.primary_screen().all_lines() {
            meta.push_back(match states.get(li) {
                // A row that straddles the emitted prefix stays pending: a duplicate
                // fragment is better than a lost one.
                Some(s) if s.done || (k + 1) * cols <= s.prefix => Row {
                    t: None,
                    done: Some(row_hash(line)),
                },
                Some(s) => Row { t: s.t, done: None },
                None => Row::default(),
            });
            if line.is_wrapped() {
                k += 1;
            } else {
                (li, k) = (li + 1, 0);
            }
        }
        self.meta = meta;
    }

    /// Marks every visible row as emitted (nothing after `finish` reaches the transcript).
    fn seal(&mut self, term: &Terminal) {
        let history = history_len(term);
        for (i, line) in term.primary_screen().visible_lines().enumerate() {
            self.row_mut(history + i).done = Some(row_hash(line));
        }
    }

    /// Emits the alt screen's lines not yet seen in this span.
    fn snapshot_alt(&mut self, opts: &RenderOptions, term: &Terminal) {
        self.alt.last_snapshot = F64::new(self.now().t);
        if !opts.alt_snapshots || !wanted(opts, self.owner) {
            return;
        }
        let now = self.now();
        let lines: Vec<(String, Stamp)> = logical_lines(term.screen().visible_lines(), String::new())
            .into_iter()
            .filter(|(_, text)| !text.is_empty() && self.alt.seen.insert(fnv1a(text)))
            .map(|(row, text)| {
                let at = self.alt.row_t.get(row).copied().flatten().map_or(now, Stamp::from);
                (text, at)
            })
            .collect();
        if !lines.is_empty() {
            self.chunker.alt(opts, self.owner, lines);
        }
    }

    /// Records first-write times for the dirty rows of the action that just ran, and makes
    /// done rows pending again when they were erased or printed over.
    fn resolve_dirty(&mut self, term: &Terminal, dirty: &mut Vec<Range<usize>>, kind: Write) {
        let now = Some(self.now);
        match term.active_screen_kind() {
            ScreenKind::Alternate => {
                if !self.alt.active {
                    self.alt.active = true;
                    self.alt.last_snapshot = F64::new(self.now().t);
                    self.alt.row_t.clear();
                }
                let rows = term.size().1;
                self.alt.row_t.resize(rows, None);
                for r in dirty.drain(..) {
                    self.alt.row_t[r.start.min(rows)..r.end.min(rows)].fill(now);
                }
            }
            ScreenKind::Primary => {
                if self.alt.active {
                    self.alt.active = false;
                    self.alt.row_t.clear();
                }
                if dirty.is_empty() {
                    return;
                }
                // Writes happen at the cursor, so rows below it are blank or were timed
                // when the cursor was on them.
                let (_, rows) = term.size();
                let cursor = term.cursor().row;
                let history = history_len(term);
                let screen = term.primary_screen();
                for range in dirty.drain(..) {
                    for i in range.start..range.end.min(rows) {
                        let Some(line) = screen.line(i) else { continue };
                        let row = self.row_mut(history + i);
                        match (kind, row.done) {
                            // An erased row is a fresh row of the current span.
                            (Write::Erase, Some(_)) => {
                                if is_blank(line) {
                                    *row = Row::default();
                                }
                            }
                            _ if i > cursor => {}
                            (Write::Print, Some(h)) => {
                                if !is_blank(line) && h != row_hash(line) {
                                    *row = Row { t: now, done: None };
                                }
                            }
                            (_, Some(_)) => {}
                            (_, None) => {
                                if row.t.is_none() && !is_blank(line) {
                                    row.t = now;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Primary rows above the visible screen that avt has not evicted yet (only non-zero in
/// the middle of a feed call).
fn history_len(term: &Terminal) -> usize {
    let total = term.primary_screen().all_lines().size_hint().0;
    total.saturating_sub(term.size().1)
}

fn leaves_alt(f: &Function<'_>) -> bool {
    matches!(f, Function::Decrst(m) if m.as_slice().iter().any(|m| matches!(m, DecMode::AltScreenBuffer | DecMode::SaveCursorAltScreenBuffer)))
}

/// The observer of one feed/resize call.
struct Feed<'a> {
    st: &'a mut State,
    opts: &'a RenderOptions,
    dirty: &'a mut Vec<Range<usize>>,
    /// Logical line states taken before a resize.
    reflow: Option<Vec<LineState>>,
}

impl Observer for Feed<'_> {
    type Output = ();

    fn before(&mut self, cx: &mut Observation<'_>, op: &Operation<'_>) {
        let term = cx.terminal();
        match op {
            Operation::Function(Function::Ris) => self.st.hard_reset(self.opts, term),
            Operation::Function(f) if term.active_screen_kind() == ScreenKind::Alternate && leaves_alt(f) => {
                self.st.snapshot_alt(self.opts, term);
            }
            Operation::Resize { .. } => self.reflow = Some(self.st.line_states(term)),
            _ => {}
        }
    }

    fn dirty(&mut self, rows: Range<usize>) {
        self.dirty.push(rows);
    }

    fn after(&mut self, cx: &mut Observation<'_>, op: &Operation<'_>) {
        let term = cx.terminal();
        if let Operation::Resize { .. } = op {
            if let Some(states) = self.reflow.take() {
                self.st.remap(term, &states);
            }
            if term.active_screen_kind() == ScreenKind::Primary {
                self.dirty.clear();
                return;
            }
        }
        self.st.resolve_dirty(term, self.dirty, write_kind(op));
    }

    fn evicted(&mut self, _cx: &mut Observation<'_>, line: &Line) {
        self.st.evict(self.opts, line);
    }

    fn end(&mut self, _cx: &mut Observation<'_>) {}
}

impl Transcript for AvtTranscript {
    fn output(&mut self, at: Stamp, data: &str) {
        self.st.now = at.into();
        // Before the feed: the content shown until now is what this event may wipe.
        if self.term.active_screen_kind() == ScreenKind::Alternate
            && at.t - self.st.alt.last_snapshot.get() >= self.opts.alt_snapshot_secs
        {
            let opts = self.opts;
            self.st.snapshot_alt(&opts, &self.term);
        }
        let mut obs = Feed {
            st: &mut self.st,
            opts: &self.opts,
            dirty: &mut self.dirty,
            reflow: None,
        };
        self.term.feed_with(data, &mut obs);
    }

    fn resize(&mut self, at: Stamp, cols: u16, rows: u16) {
        // A size no terminal has (a broken `stty`, the old plugin's 3350-column resizes) is
        // dropped, as the web player drops it: the screen keeps its size.
        let Some((cols, rows)) = sane_size(cols, rows) else {
            return;
        };
        if (cols, rows) == self.size() {
            return;
        }
        self.st.now = at.into();
        let opts = self.opts;
        self.st.end_pending(&opts);
        self.st.chunker.close();
        let mut obs = Feed {
            st: &mut self.st,
            opts: &self.opts,
            dirty: &mut self.dirty,
            reflow: None,
        };
        self.term.resize_with(usize::from(cols), usize::from(rows), &mut obs);
        self.st.cols = cols;
        self.st.rows = rows;
        if self.st.alt.active {
            self.st.alt.row_t.resize(usize::from(rows), None);
        }
    }

    fn boundary(&mut self, at: Stamp, next: Owner) {
        self.st.now = at.into();
        let cut = match self.term.active_screen_kind() {
            ScreenKind::Primary => self.st.base + self.term.cursor().row as u64,
            // The primary cursor is not visible behind the alt screen: the span keeps
            // everything written to the primary screen so far.
            ScreenKind::Alternate => self.primary_end(),
        };
        self.close_span(cut);
        self.st.owner = next;
        self.st.alt.seen.clear();
    }

    fn owner(&self) -> Owner {
        self.st.owner
    }

    fn lines_above_cursor(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }
        let upto = match self.term.active_screen_kind() {
            ScreenKind::Primary => self.term.cursor().row,
            ScreenKind::Alternate => usize::try_from(self.primary_end() - self.st.base).unwrap_or(0),
        };
        let rows = self.term.primary_screen().visible_lines().take(upto);
        // A row that scrolled off mid soft-wrap continues on the first visible row.
        let visible = logical_lines(rows, self.st.recent.partial.clone());
        let mut out: Vec<String> = self.st.recent.lines.iter().cloned().collect();
        out.extend(visible.into_iter().map(|(_, t)| t));
        let skip = out.len().saturating_sub(max_lines);
        out.split_off(skip)
    }

    fn is_alt_screen(&self) -> bool {
        self.term.active_screen_kind() == ScreenKind::Alternate
    }

    fn take_chunks(&mut self) -> Vec<Chunk> {
        self.st.chunker.take()
    }

    fn provisional(&self) -> Vec<Chunk> {
        let owner = self.st.owner;
        if !wanted(&self.opts, owner) {
            return Vec::new();
        }
        let open = self.st.chunker.open_of(owner);
        let mut at = open.map(|(_, at, _)| at);
        let mut text = open.map(|(_, _, t)| t.to_owned()).unwrap_or_default();
        let mut add = |line: &str, when: Stamp| {
            if text.is_empty() && line.is_empty() {
                return;
            }
            at.get_or_insert(when);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line);
        };
        let end = match self.term.active_screen_kind() {
            ScreenKind::Primary => self.primary_end().max(self.st.base + self.term.cursor().row as u64 + 1),
            ScreenKind::Alternate => self.primary_end(),
        };
        let take = usize::try_from(end - self.st.base).unwrap_or(usize::MAX);
        // Pending rows joined into logical lines; done rows end a line.
        let mut cur = self.st.pending.as_ref().map(|p| (Stamp::from(p.at), p.text.clone()));
        let mut lines = Vec::new();
        for (abs, line) in (self.st.base..).zip(self.term.primary_screen().visible_lines().take(take)) {
            if self.st.row(abs).done.is_some() {
                lines.extend(cur.take());
                continue;
            }
            let c = cur.get_or_insert_with(|| (self.st.row_time(abs), String::new()));
            c.1.extend(line.chars());
            if !line.is_wrapped() {
                lines.extend(cur.take());
            }
        }
        lines.extend(cur);
        for (when, line) in lines {
            add(line.trim_end(), when);
        }
        if self.is_alt_screen() {
            for (i, line) in logical_lines(self.term.screen().visible_lines(), String::new()) {
                if !line.is_empty() {
                    let when = self
                        .st
                        .alt
                        .row_t
                        .get(i)
                        .copied()
                        .flatten()
                        .map_or(self.st.now(), Stamp::from);
                    add(&line, when);
                }
            }
        }
        text.truncate(text.trim_end().len());
        let Some(at) = at.filter(|_| !text.is_empty()) else {
            return Vec::new();
        };
        vec![Chunk {
            kind: ChunkKind::Provisional,
            cmd_seq: owner.cmd_seq(),
            t: at.t,
            ts: at.ts,
            line0: open.map_or_else(|| self.st.chunker.next_line(owner), |(l, _, _)| l),
            text,
        }]
    }

    fn finish(&mut self, at: Stamp) -> Vec<Chunk> {
        self.st.now = at.into();
        let cut = self.primary_end();
        self.close_span(cut);
        self.st.seal(&self.term);
        self.st.chunker.take()
    }

    fn output_stats(&self, owner: Owner) -> OutputStats {
        self.st.chunker.stats(owner)
    }

    fn snapshot(&self) -> TranscriptState {
        TranscriptState {
            vt_dump: self.term.dump(),
            evicted: self.st.base,
            state: serde_json::to_string(&self.st).expect("transcript state serializes"),
        }
    }
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
