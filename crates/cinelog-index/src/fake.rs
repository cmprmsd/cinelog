//! In-memory reference implementations of the M2 traits for tests of the other work
//! packages (`features = ["fake"]`). Semantics follow the trait docs, simplified:
//! [`MemIndex`] searches by case-folded substring without FTS; [`PlainTranscript`] treats
//! output as plain lines (escape sequences dropped) and a `rows`-line screen.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::api::{IndexWriter, MetaStore, Searcher, Transcript};
use crate::error::{IndexError, Result};
use crate::model::*;

// ------------------------------------------------------------------ MemIndex

#[derive(Debug, Default)]
struct State {
    next_id: i64,
    sessions: BTreeMap<String, SessionMeta>,
    parts: BTreeMap<(String, u32), CastPart>,
    commands: BTreeMap<(String, Seq), (i64, Command)>,
    outputs: Vec<(i64, String, Chunk)>,
    markers: Vec<(String, MarkerRow)>,
    /// (session, minute) → (inputs, out_bytes).
    activity: BTreeMap<(String, i64), (u64, u64)>,
    segments: BTreeMap<(String, u32), Segment>,
    checkpoints: BTreeMap<String, (Checkpoint, f64)>,
    frames: BTreeMap<(String, u32), Vec<ZstFrameRow>>,
    legacy: BTreeMap<PathBuf, LegacyImportRow>,
    info: BTreeMap<String, String>,
    forgotten_applied: Vec<Forgotten>,
    // meta.db
    forgotten: Vec<Forgotten>,
    settings: BTreeMap<String, String>,
    annotations: BTreeMap<String, Annotation>,
    /// Tag name → colour.
    tags: BTreeMap<String, Option<String>>,
}

impl State {
    fn id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    fn drop_session_rows(&mut self, uuid: &str) {
        self.parts.retain(|(u, _), _| u != uuid);
        self.commands.retain(|(u, _), _| u != uuid);
        self.outputs.retain(|(_, u, _)| u != uuid);
        self.markers.retain(|(u, _)| u != uuid);
        self.activity.retain(|(u, _), _| u != uuid);
        self.segments.retain(|(u, _), _| u != uuid);
        self.checkpoints.remove(uuid);
        self.frames.retain(|(u, _), _| u != uuid);
    }

    fn command_forgotten(&self, uuid: &str, seq: Seq) -> bool {
        self.forgotten_applied.iter().any(|f| match &f.target {
            ForgetTarget::Session { uuid: u } => u == uuid,
            ForgetTarget::Command { uuid: u, seq: s } => u == uuid && *s == seq,
            ForgetTarget::Range { .. } => false,
        }) || self.forgotten.iter().any(|f| match &f.target {
            ForgetTarget::Session { uuid: u } => u == uuid,
            ForgetTarget::Command { uuid: u, seq: s } => u == uuid && *s == seq,
            ForgetTarget::Range { .. } => false,
        })
    }

    fn chunk_forgotten(&self, uuid: &str, chunk: &Chunk) -> bool {
        let lists = self.forgotten_applied.iter().chain(self.forgotten.iter());
        lists.into_iter().any(|f| match &f.target {
            ForgetTarget::Session { uuid: u } => u == uuid,
            ForgetTarget::Command { uuid: u, seq } => u == uuid && chunk.cmd_seq == Some(*seq),
            ForgetTarget::Range { uuid: u, t_from, t_to } => u == uuid && chunk.t >= *t_from && chunk.t <= *t_to,
        })
    }

    /// The targets of the newest [`TAG_FILTER_CAP`] non-deleted annotations carrying every
    /// tag of `tags` (`None`: no tag filter), and whether there were more.
    fn tagged(&self, tags: &[String]) -> (Option<Tagged>, bool) {
        if tags.is_empty() {
            return (None, false);
        }
        let mut found: Vec<&Annotation> = self
            .annotations
            .values()
            .filter(|a| a.deleted_at.is_none() && tags.iter().all(|t| a.tags.contains(t)))
            .collect();
        found.sort_by(|a, b| b.created_at.total_cmp(&a.created_at).then_with(|| b.uuid.cmp(&a.uuid)));
        let capped = found.len() > TAG_FILTER_CAP;
        found.truncate(TAG_FILTER_CAP);
        let mut t = Tagged::default();
        for a in found {
            match (a.cmd_seq, a.t_from, a.t_to) {
                (Some(seq), _, _) => t.commands.push((a.session_uuid.clone(), seq)),
                (None, Some(from), Some(to)) => t.spans.push((a.session_uuid.clone(), from, to)),
                _ => {}
            }
        }
        (Some(t), capped)
    }

    /// The tags of the non-deleted annotations on a command, sorted and distinct.
    fn command_tags(&self, uuid: &str, seq: Option<Seq>) -> Vec<String> {
        let Some(seq) = seq else { return Vec::new() };
        let mut tags: Vec<String> = self
            .annotations
            .values()
            .filter(|a| a.deleted_at.is_none() && a.session_uuid == uuid && a.cmd_seq == Some(seq))
            .flat_map(|a| a.tags.iter().cloned())
            .collect();
        tags.sort_unstable();
        tags.dedup();
        tags
    }

    fn session_forgotten(&self, uuid: &str) -> bool {
        self.forgotten_applied
            .iter()
            .chain(self.forgotten.iter())
            .any(|f| matches!(&f.target, ForgetTarget::Session { uuid: u } if u == uuid))
    }

    fn purge(&mut self, list: &[Forgotten]) -> ForgetOutcome {
        let mut out = ForgetOutcome::default();
        for f in list {
            match &f.target {
                ForgetTarget::Session { uuid } => {
                    if self.sessions.remove(uuid).is_some() {
                        out.sessions += 1;
                    }
                    out.commands += self.commands.keys().filter(|(u, _)| u == uuid).count() as u64;
                    out.outputs += self.outputs.iter().filter(|(_, u, _)| u == uuid).count() as u64;
                    self.drop_session_rows(uuid);
                }
                ForgetTarget::Command { uuid, seq } => {
                    if self.commands.remove(&(uuid.clone(), *seq)).is_some() {
                        out.commands += 1;
                    }
                    let before = self.outputs.len();
                    self.outputs.retain(|(_, u, c)| !(u == uuid && c.cmd_seq == Some(*seq)));
                    out.outputs += (before - self.outputs.len()) as u64;
                }
                ForgetTarget::Range { uuid, t_from, t_to } => {
                    let before = self.outputs.len();
                    self.outputs
                        .retain(|(_, u, c)| !(u == uuid && c.t >= *t_from && c.t <= *t_to));
                    out.outputs += (before - self.outputs.len()) as u64;
                }
            }
        }
        out
    }

    fn summary(&self, s: &SessionMeta) -> SessionSummary {
        SessionSummary {
            uuid: s.uuid.clone(),
            host: s.host.clone(),
            user: s.user.clone(),
            scope: s.scope,
            state: s.state,
            started_at: s.started_at,
            ended_at: s.ended_at,
            cast_path: s.cast_path.clone(),
            tmux: s.tmux.clone(),
            legacy: s.legacy,
            hidden: s.hidden,
            commands: self.commands.keys().filter(|(u, _)| *u == s.uuid).count() as u64,
        }
    }

    fn output_of(&self, uuid: &str, pick: impl Fn(&Chunk) -> bool, max: usize) -> (String, bool) {
        let mut chunks: Vec<&Chunk> = self
            .outputs
            .iter()
            .filter(|(_, u, c)| u == uuid && pick(c))
            .map(|(_, _, c)| c)
            .collect();
        chunks.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.line0.cmp(&b.line0)));
        let mut text = String::new();
        for c in chunks {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&c.text);
        }
        if text.len() <= max {
            return (text, false);
        }
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        (text, true)
    }
}

/// Shared in-memory index + meta store. Clones share state, so one clone can be the writer
/// and another the reader.
#[derive(Debug, Clone, Default)]
pub struct MemIndex {
    inner: Arc<Mutex<State>>,
}

impl MemIndex {
    pub fn new() -> Self {
        Self::default()
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// All final and provisional chunks of a session, in insertion order.
    pub fn chunks(&self, uuid: &str) -> Vec<Chunk> {
        self.st()
            .outputs
            .iter()
            .filter(|(_, u, _)| u == uuid)
            .map(|(_, _, c)| c.clone())
            .collect()
    }

    pub fn commands(&self, uuid: &str) -> Vec<Command> {
        self.st()
            .commands
            .iter()
            .filter(|((u, _), _)| u == uuid)
            .map(|(_, (_, c))| c.clone())
            .collect()
    }

    /// Activity of a session, by minute.
    pub fn activity(&self, uuid: &str) -> Vec<ActivityBin> {
        self.st()
            .activity
            .iter()
            .filter(|((u, _), _)| u == uuid)
            .map(|((_, minute), (inputs, out_bytes))| ActivityBin {
                minute: *minute,
                inputs: *inputs,
                out_bytes: *out_bytes,
            })
            .collect()
    }

    pub fn markers(&self, uuid: &str) -> Vec<MarkerRow> {
        self.st()
            .markers
            .iter()
            .filter(|(u, _)| u == uuid)
            .map(|(_, m)| m.clone())
            .collect()
    }

    pub fn sessions(&self) -> Vec<SessionMeta> {
        self.st().sessions.values().cloned().collect()
    }

    pub fn segments(&self, uuid: &str) -> Vec<Segment> {
        self.st()
            .segments
            .iter()
            .filter(|((u, _), _)| u == uuid)
            .map(|(_, s)| s.clone())
            .collect()
    }

    pub fn info(&self, key: &str) -> Option<String> {
        self.st().info.get(key).cloned()
    }

    pub fn parts(&self, uuid: &str) -> Vec<CastPart> {
        self.st()
            .parts
            .iter()
            .filter(|((u, _), _)| u == uuid)
            .map(|(_, p)| p.clone())
            .collect()
    }

    pub fn frames(&self, uuid: &str, part: u32) -> Option<Vec<ZstFrameRow>> {
        self.st().frames.get(&(uuid.to_owned(), part)).cloned()
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl IndexWriter for MemIndex {
    fn apply(&mut self, b: &IndexBatch) -> Result<()> {
        let mut st = self.st();
        let uuid = b.session.uuid.clone();
        if st.session_forgotten(&uuid) {
            return Ok(());
        }
        if b.reset {
            st.drop_session_rows(&uuid);
        }
        // Like the real writer: another session at this path (a replaced cast, or a legacy
        // cast that grew and hashes to a new uuid) is stale.
        let stale: Vec<String> = st
            .sessions
            .iter()
            .filter(|(u, s)| **u != uuid && s.cast_path == b.session.cast_path)
            .map(|(u, _)| u.clone())
            .collect();
        for u in stale {
            st.drop_session_rows(&u);
            st.sessions.remove(&u);
        }
        st.sessions.insert(uuid.clone(), b.session.clone());
        for p in &b.parts {
            if let Some(frames) = &p.frames {
                st.frames.insert((uuid.clone(), p.part), frames.clone());
            }
            st.parts.insert((uuid.clone(), p.part), p.clone());
        }
        for c in &b.commands {
            if st.command_forgotten(&uuid, c.seq) {
                continue;
            }
            let key = (uuid.clone(), c.seq);
            let id = match st.commands.get(&key) {
                Some((id, _)) => *id,
                None => st.id(),
            };
            st.commands.insert(key, (id, c.clone()));
        }
        if b.provisional.is_some() || b.finished {
            st.outputs
                .retain(|(_, u, c)| !(*u == uuid && c.kind == ChunkKind::Provisional));
        }
        let provisional = if b.finished { None } else { b.provisional.as_ref() };
        for c in b.chunks.iter().chain(provisional.into_iter().flatten()) {
            if st.chunk_forgotten(&uuid, c) {
                continue;
            }
            let id = st.id();
            st.outputs.push((id, uuid.clone(), c.clone()));
        }
        for m in &b.markers {
            st.markers.push((uuid.clone(), m.clone()));
        }
        for a in &b.activity {
            let e = st.activity.entry((uuid.clone(), a.minute)).or_default();
            e.0 += a.inputs;
            e.1 += a.out_bytes;
        }
        for s in &b.segments {
            st.segments.insert((uuid.clone(), s.seq), s.clone());
        }
        if let Some(cp) = &b.checkpoint {
            st.checkpoints.insert(uuid.clone(), (cp.clone(), now()));
        }
        if let Some(l) = &b.legacy {
            let mut l = l.clone();
            l.session_uuid.get_or_insert_with(|| uuid.clone());
            st.legacy.insert(l.path.clone(), l);
        }
        Ok(())
    }

    fn apply_forgotten(&mut self, forgotten: &[Forgotten]) -> Result<ForgetOutcome> {
        let mut st = self.st();
        st.forgotten_applied = forgotten.to_vec();
        Ok(st.purge(forgotten))
    }

    fn record_legacy(&mut self, row: &LegacyImportRow) -> Result<()> {
        self.st().legacy.insert(row.path.clone(), row.clone());
        Ok(())
    }

    fn record_compression(&mut self, u: &CompressionUpdate) -> Result<()> {
        let mut st = self.st();
        let key = (u.uuid.clone(), u.part);
        let Some(part) = st.parts.get_mut(&key) else {
            return Err(IndexError::NotFound(format!("{} part {}", u.uuid, u.part)));
        };
        part.path = u.to.clone();
        part.compressed = true;
        part.bytes = Some(u.uncompressed_bytes);
        let all = st
            .parts
            .iter()
            .filter(|((id, _), _)| *id == u.uuid)
            .all(|(_, p)| p.compressed);
        if let Some(s) = st.sessions.get_mut(&u.uuid) {
            if u.part == 1 {
                s.cast_path = u.to.clone();
            }
            s.compressed = all;
        }
        st.frames.insert(key, u.frames.clone());
        Ok(())
    }

    fn set_info(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let mut st = self.st();
        match value {
            Some(v) => st.info.insert(key.to_owned(), v.to_owned()),
            None => st.info.remove(key),
        };
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        let mut st = self.st();
        let uuids: Vec<String> = st.sessions.keys().cloned().collect();
        for u in uuids {
            st.drop_session_rows(&u);
        }
        st.sessions.clear();
        st.legacy.clear();
        Ok(())
    }

    fn known_casts(&self) -> Result<Vec<KnownCast>> {
        let st = self.st();
        Ok(st
            .sessions
            .values()
            .map(|s| {
                let cp = st.checkpoints.get(&s.uuid).map(|(c, _)| c);
                KnownCast {
                    uuid: s.uuid.clone(),
                    cast_path: s.cast_path.clone(),
                    legacy: s.legacy,
                    state: s.state,
                    parts: s.parts,
                    compressed: s.compressed,
                    ended_at: s.ended_at,
                    indexed_part: cp.map(|c| c.part),
                    indexed_offset: cp.map(|c| c.byte_offset),
                    indexed_file: cp.map(|c| c.file),
                }
            })
            .collect())
    }

    fn checkpoint(&self, uuid: &str) -> Result<Option<Checkpoint>> {
        Ok(self.st().checkpoints.get(uuid).map(|(c, _)| c.clone()))
    }

    fn legacy_entry(&self, path: &Path) -> Result<Option<LegacyImportRow>> {
        Ok(self.st().legacy.get(path).cloned())
    }

    fn compress_candidates(&self, ended_before: f64) -> Result<Vec<CompressCandidate>> {
        let st = self.st();
        let mut out = Vec::new();
        for ((uuid, part), p) in &st.parts {
            let Some(s) = st.sessions.get(uuid) else { continue };
            let finished = matches!(s.state, SessionState::Ended | SessionState::Crashed);
            if let (true, false, false, Some(ended)) = (finished, s.legacy, p.compressed, s.ended_at)
                && ended < ended_before
            {
                out.push(CompressCandidate {
                    uuid: uuid.clone(),
                    part: *part,
                    path: p.path.clone(),
                    ended_at: ended,
                });
            }
        }
        Ok(out)
    }
}

fn fold(s: &str, case_sensitive: bool) -> String {
    if case_sensitive { s.to_owned() } else { s.to_lowercase() }
}

/// Text around the first match of `needle` (already folded) with snippet sentinels.
fn snippet(text: &str, needle: &str, case_sensitive: bool) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<String> = chars.iter().map(|c| fold(&c.to_string(), case_sensitive)).collect();
    let n: Vec<char> = needle.chars().collect();
    let hay: String = folded.concat();
    let byte = hay.find(needle)?;
    // Map the folded byte offset back to a char index (folding may change lengths).
    let mut acc = 0;
    let start = folded.iter().position(|f| {
        let hit = acc >= byte;
        acc += f.len();
        hit
    })?;
    let end = (start + n.len()).min(chars.len());
    let from = start.saturating_sub(32);
    let to = (end + 32).min(chars.len());
    let mut s: String = chars[from..start].iter().collect();
    s.push(SNIPPET_OPEN);
    s.extend(&chars[start..end]);
    s.push(SNIPPET_CLOSE);
    s.extend(&chars[end..to]);
    Some(s)
}

/// What a search tag filter admits: annotated commands and annotated spans.
#[derive(Debug, Default)]
struct Tagged {
    commands: Vec<(String, Seq)>,
    spans: Vec<(String, f64, f64)>,
}

impl Tagged {
    fn command(&self, uuid: &str, c: &Command) -> bool {
        self.commands.iter().any(|(u, s)| u == uuid && *s == c.seq)
            || self
                .spans
                .iter()
                .any(|(u, from, to)| u == uuid && c.t_start <= *to && c.t_end.is_none_or(|e| e >= *from))
    }

    fn chunk(&self, uuid: &str, ch: &Chunk) -> bool {
        ch.cmd_seq
            .is_some_and(|seq| self.commands.iter().any(|(u, s)| u == uuid && *s == seq))
            || self
                .spans
                .iter()
                .any(|(u, from, to)| u == uuid && ch.t >= *from && ch.t <= *to)
    }
}

fn passes(f: &SearchFilters, s: &SessionMeta, ts: f64, cmd: Option<&Command>) -> bool {
    let eq = |want: &Option<String>, have: Option<&str>| want.as_deref().is_none_or(|w| have == Some(w));
    (f.include_hidden || !s.hidden)
        && eq(&f.user, cmd.and_then(|c| c.user.as_deref()).or(Some(&s.user)))
        && eq(&f.host, Some(&s.host))
        && eq(&f.session, Some(&s.uuid))
        && eq(&f.pane, s.tmux.pane.as_deref())
        && f.since.is_none_or(|v| ts >= v)
        && f.until.is_none_or(|v| ts <= v)
        && f.exit.is_none_or(|e| e.matches(cmd.and_then(|c| c.exit)))
        && eq(&f.cwd, cmd.and_then(|c| c.cwd.as_deref()))
}

impl Searcher for MemIndex {
    fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        let st = self.st();
        let (tagged, tags_capped) = st.tagged(&q.filters.tags);
        let needle = fold(&q.term, q.case_sensitive);
        let mut hits = Vec::new();
        let by_seq = |uuid: &str, seq: Option<Seq>| seq.and_then(|s| st.commands.get(&(uuid.to_owned(), s)));
        if q.target != SearchTarget::Output {
            for ((uuid, _), (id, c)) in &st.commands {
                let Some(s) = st.sessions.get(uuid) else { continue };
                let hay = [Some(c.typed.as_str()), c.expanded.as_deref(), c.cwd.as_deref()];
                let hit = hay
                    .iter()
                    .flatten()
                    .any(|h| fold(h, q.case_sensitive).contains(&needle));
                if hit
                    && passes(&q.filters, s, c.ts_start, Some(c))
                    && !st.command_forgotten(uuid, c.seq)
                    && tagged.as_ref().is_none_or(|t| t.command(uuid, c))
                {
                    hits.push(SearchHit {
                        kind: HitKind::Command,
                        id: *id,
                        session_uuid: uuid.clone(),
                        cmd_seq: Some(c.seq),
                        ts: c.ts_start,
                        t: c.t_start,
                        user: c.user.clone().unwrap_or_else(|| s.user.clone()),
                        host: s.host.clone(),
                        exit: c.exit,
                        cwd: c.cwd.clone(),
                        typed: Some(c.typed.clone()),
                        snippet: None,
                        chunk_kind: None,
                        approx: c.approx,
                        confidence: c.confidence,
                        tags: Vec::new(),
                    });
                }
            }
        }
        if q.target != SearchTarget::Commands && !needle.is_empty() {
            for (id, uuid, ch) in &st.outputs {
                let Some(s) = st.sessions.get(uuid) else { continue };
                let cmd = by_seq(uuid, ch.cmd_seq).map(|(_, c)| c);
                if !fold(&ch.text, q.case_sensitive).contains(&needle)
                    || !passes(&q.filters, s, ch.ts, cmd)
                    || st.chunk_forgotten(uuid, ch)
                    || tagged.as_ref().is_some_and(|t| !t.chunk(uuid, ch))
                {
                    continue;
                }
                hits.push(SearchHit {
                    kind: HitKind::Output,
                    id: *id,
                    session_uuid: uuid.clone(),
                    cmd_seq: ch.cmd_seq,
                    ts: ch.ts,
                    t: ch.t,
                    user: cmd.and_then(|c| c.user.clone()).unwrap_or_else(|| s.user.clone()),
                    host: s.host.clone(),
                    exit: cmd.and_then(|c| c.exit),
                    cwd: cmd.and_then(|c| c.cwd.clone()),
                    typed: cmd.map(|c| c.typed.clone()),
                    snippet: snippet(&ch.text, &needle, q.case_sensitive),
                    chunk_kind: Some(ch.kind),
                    approx: cmd.is_some_and(|c| c.approx),
                    confidence: cmd.and_then(|c| c.confidence),
                    tags: Vec::new(),
                });
            }
        }
        let key = |h: &SearchHit| (h.ts, h.kind == HitKind::Output, h.id);
        hits.sort_by(|a, b| {
            let (a, b) = (key(a), key(b));
            b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2))
        });
        if let Some(c) = q.cursor {
            let ck = (c.ts, c.kind == HitKind::Output, c.id);
            hits.retain(|h| {
                let k = key(h);
                k.0 < ck.0 || (k.0 == ck.0 && (k.1, k.2) < (ck.1, ck.2))
            });
        }
        let capped = hits.len() > CANDIDATE_CAP || tags_capped;
        let more = hits.len() > q.limit;
        hits.truncate(q.limit);
        for h in &mut hits {
            h.tags = st.command_tags(&h.session_uuid, h.cmd_seq);
        }
        let next = more.then(|| hits.last()).flatten().map(|h| Cursor {
            ts: h.ts,
            kind: h.kind,
            id: h.id,
        });
        Ok(SearchPage {
            hits,
            next,
            partial: false,
            capped,
        })
    }

    fn session(&self, uuid: &str) -> Result<Option<SessionSummary>> {
        let st = self.st();
        Ok(st.sessions.get(uuid).map(|s| st.summary(s)))
    }

    fn show(&self, entry: &EntryRef, max: usize) -> Result<Option<EntryDetail>> {
        let st = self.st();
        let Some(s) = st.sessions.get(entry.uuid()) else {
            return Ok(None);
        };
        let session = st.summary(s);
        let uuid = entry.uuid();
        let detail = match entry {
            EntryRef::Session { .. } => {
                let (output, output_truncated) = st.output_of(uuid, |_| true, max);
                EntryDetail {
                    session,
                    command: None,
                    output,
                    output_truncated,
                }
            }
            EntryRef::Command { seq, .. } => {
                let Some((_, c)) = st.commands.get(&(uuid.to_owned(), *seq)) else {
                    return Ok(None);
                };
                let (output, output_truncated) = st.output_of(uuid, |ch| ch.cmd_seq == Some(*seq), max);
                EntryDetail {
                    session,
                    command: Some(c.clone()),
                    output,
                    output_truncated,
                }
            }
            EntryRef::At { t, .. } => {
                let running = st
                    .commands
                    .iter()
                    .filter(|((u, _), (_, c))| u == uuid && c.t_start <= *t && c.t_end.is_none_or(|e| e >= *t))
                    .map(|(_, (_, c))| c)
                    .max_by(|a, b| a.depth.cmp(&b.depth));
                let seq = running.map(|c| c.seq);
                let (output, output_truncated) = st.output_of(uuid, |ch| ch.cmd_seq == seq, max);
                EntryDetail {
                    session,
                    command: running.cloned(),
                    output,
                    output_truncated,
                }
            }
        };
        Ok(Some(detail))
    }

    fn status(&self) -> Result<IndexStatus> {
        let st = self.st();
        let mut counts = SessionCounts::default();
        let mut live = Vec::new();
        let mut backlog = 0;
        for s in st.sessions.values() {
            match s.state {
                SessionState::Live => counts.live += 1,
                SessionState::Ended => counts.ended += 1,
                SessionState::Crashed => counts.crashed += 1,
                SessionState::Degraded => counts.degraded += 1,
                SessionState::Error => counts.error += 1,
            }
            counts.legacy += u64::from(s.legacy);
            counts.hidden += u64::from(s.hidden);
            let cp = st.checkpoints.get(&s.uuid);
            let indexed = cp.map_or(0, |(c, _)| c.byte_offset);
            let size = std::fs::metadata(&s.cast_path).ok().map(|m| m.len());
            let imported = st.legacy.values().any(|l| l.session_uuid.as_ref() == Some(&s.uuid));
            if !imported {
                backlog += size.unwrap_or(0).saturating_sub(indexed);
            }
            if s.state == SessionState::Live {
                live.push(LiveLag {
                    uuid: s.uuid.clone(),
                    cast_path: s.cast_path.clone(),
                    indexed_bytes: indexed,
                    file_bytes: size,
                    updated_at: cp.map(|(_, at)| *at),
                });
            }
        }
        let mut legacy = LegacyProgress::default();
        for l in st.legacy.values() {
            legacy.files += 1;
            match l.status {
                LegacyStatus::Imported => legacy.imported += 1,
                LegacyStatus::Hidden => legacy.hidden += 1,
                LegacyStatus::Skipped => legacy.skipped += 1,
                LegacyStatus::Failed => legacy.failed += 1,
            }
            legacy.banners += l.banners;
            legacy.log_lines += l.log_lines;
            legacy.aligned += l.aligned;
        }
        Ok(IndexStatus {
            schema_version: crate::schema::INDEX_SCHEMA_VERSION,
            index_bytes: 0,
            sessions: counts,
            commands: st.commands.len() as u64,
            outputs: st.outputs.len() as u64,
            live,
            backlog_bytes: backlog,
            legacy,
            last_error: st.info.get(info_keys::LAST_ERROR).cloned(),
            info: st.info.clone(),
        })
    }

    fn legacy_report(&self) -> Result<Vec<LegacyImportRow>> {
        Ok(self.st().legacy.values().cloned().collect())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "table", content = "row", rename_all = "lowercase")]
enum MetaLine {
    Annotations(Annotation),
    Forgotten(Forgotten),
    Settings { key: String, value: String },
}

impl MetaStore for MemIndex {
    fn forget(&mut self, target: &ForgetTarget, by: &str) -> Result<Forgotten> {
        if let ForgetTarget::Range { t_from, t_to, .. } = target
            && t_from > t_to
        {
            return Err(IndexError::Invalid(format!("range {t_from}..{t_to}")));
        }
        let mut st = self.st();
        if let Some(f) = st.forgotten.iter().find(|f| f.target == *target) {
            return Ok(f.clone());
        }
        let f = Forgotten {
            target: target.clone(),
            by: by.to_owned(),
            at: now(),
        };
        st.forgotten.push(f.clone());
        Ok(f)
    }

    fn forgotten(&self) -> Result<Vec<Forgotten>> {
        Ok(self.st().forgotten.clone())
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.st().settings.get(key).cloned())
    }

    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        let mut st = self.st();
        match value {
            Some(v) => st.settings.insert(key.to_owned(), v.to_owned()),
            None => st.settings.remove(key),
        };
        Ok(())
    }

    fn put_annotation(&mut self, a: &Annotation) -> Result<()> {
        let mut st = self.st();
        let mut a = a.clone();
        a.tags = a
            .tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        a.tags.sort();
        a.tags.dedup();
        for t in &a.tags {
            st.tags.entry(t.clone()).or_default();
        }
        st.annotations.insert(a.uuid.clone(), a);
        Ok(())
    }

    fn annotations(&self, session_uuid: &str) -> Result<Vec<Annotation>> {
        let mut v: Vec<Annotation> = self
            .st()
            .annotations
            .values()
            .filter(|a| a.session_uuid == session_uuid)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.created_at.total_cmp(&b.created_at));
        Ok(v)
    }

    fn annotation(&self, uuid: &str) -> Result<Option<Annotation>> {
        Ok(self.st().annotations.get(uuid).cloned())
    }

    fn find_annotations(&self, q: &AnnotationQuery) -> Result<Vec<Annotation>> {
        let st = self.st();
        let needle = q.text.as_ref().map(|t| t.to_lowercase());
        let key = |a: &Annotation| (a.created_at, a.uuid.clone());
        let after = |a: &Annotation| {
            q.after.as_ref().is_none_or(|(c, u)| {
                let ord = a.created_at.total_cmp(c).then_with(|| a.uuid.as_str().cmp(u));
                if q.newest_first { ord.is_lt() } else { ord.is_gt() }
            })
        };
        let mut v: Vec<Annotation> = st
            .annotations
            .values()
            .filter(|a| q.sessions.is_empty() || q.sessions.contains(&a.session_uuid))
            .filter(|a| q.cmd_seq.is_none_or(|s| a.cmd_seq == Some(s)))
            .filter(|a| {
                q.commands.is_empty()
                    || a.cmd_seq
                        .is_some_and(|s| q.commands.iter().any(|(u, n)| *u == a.session_uuid && *n == s))
            })
            .filter(|a| q.uuid.as_ref().is_none_or(|u| a.uuid == *u))
            .filter(|a| q.tags.iter().all(|t| a.tags.contains(t)))
            .filter(|a| q.author.as_ref().is_none_or(|w| a.author == *w))
            .filter(|a| needle.as_ref().is_none_or(|n| a.body.to_lowercase().contains(n)))
            .filter(|a| q.since.is_none_or(|v| a.created_at >= v) && q.until.is_none_or(|v| a.created_at <= v))
            .filter(|a| q.include_deleted || a.deleted_at.is_none())
            .filter(|a| after(a))
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            let (a, b) = (key(a), key(b));
            a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1))
        });
        if q.newest_first {
            v.reverse();
        }
        if q.limit > 0 {
            v.truncate(q.limit);
        }
        Ok(v)
    }

    fn tags(&self) -> Result<Vec<TagInfo>> {
        let st = self.st();
        Ok(st
            .tags
            .iter()
            .map(|(name, color)| TagInfo {
                name: name.clone(),
                color: color.clone(),
                count: st
                    .annotations
                    .values()
                    .filter(|a| a.deleted_at.is_none() && a.tags.contains(name))
                    .count() as u64,
            })
            .collect())
    }

    fn authors(&self) -> Result<Vec<AuthorInfo>> {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for a in self.st().annotations.values().filter(|a| a.deleted_at.is_none()) {
            *counts.entry(a.author.clone()).or_default() += 1;
        }
        Ok(counts
            .into_iter()
            .map(|(name, count)| AuthorInfo { name, count })
            .collect())
    }

    fn put_tag(&mut self, name: &str, color: Option<&str>) -> Result<TagInfo> {
        let name = name.trim();
        if name.is_empty() {
            return Err(IndexError::Invalid("empty tag name".into()));
        }
        self.st().tags.insert(name.to_owned(), color.map(str::to_owned));
        self.tags()?
            .into_iter()
            .find(|t| t.name == name)
            .ok_or_else(|| IndexError::NotFound(format!("tag {name:?}")))
    }

    fn export_jsonl(&self, out: &mut dyn Write) -> Result<u64> {
        let st = self.st();
        let lines = st
            .annotations
            .values()
            .cloned()
            .map(MetaLine::Annotations)
            .chain(st.forgotten.iter().cloned().map(MetaLine::Forgotten))
            .chain(st.settings.iter().map(|(k, v)| MetaLine::Settings {
                key: k.clone(),
                value: v.clone(),
            }));
        let mut n = 0;
        for l in lines {
            serde_json::to_writer(&mut *out, &l)?;
            out.write_all(b"\n")?;
            n += 1;
        }
        Ok(n)
    }

    fn import_jsonl(&mut self, input: &mut dyn BufRead) -> Result<u64> {
        let mut n = 0;
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MetaLine>(&line)? {
                MetaLine::Annotations(a) => self.put_annotation(&a)?,
                MetaLine::Forgotten(f) => {
                    let mut st = self.st();
                    if !st.forgotten.iter().any(|g| g.target == f.target) {
                        st.forgotten.push(f);
                    }
                }
                MetaLine::Settings { key, value } => self.set_setting(&key, Some(&value))?,
            }
            n += 1;
        }
        Ok(n)
    }
}

// ------------------------------------------------------------------ PlainTranscript

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Line {
    text: String,
    at: Stamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenChunk {
    owner: Owner,
    line0: u64,
    at: Stamp,
    text: String,
    lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlainState {
    rows: usize,
    owner: Owner,
    /// Completed on-screen lines, oldest first, all owned by `owner`.
    screen: VecDeque<Line>,
    partial: Option<Line>,
    open: Option<OpenChunk>,
    done: Vec<Chunk>,
    stats: Vec<(Owner, OutputStats)>,
    recent: VecDeque<String>,
    alt: bool,
    evicted: u64,
    /// Inside an escape sequence: 0 none, 1 after ESC, 2 CSI, 3 OSC/string.
    esc: u8,
    csi: String,
}

/// Plain-text stand-in for the avt renderer (see module docs).
#[derive(Debug, Clone)]
pub struct PlainTranscript {
    opts: RenderOptions,
    s: PlainState,
}

impl PlainTranscript {
    pub fn new(_cols: u16, rows: u16, opts: RenderOptions) -> Self {
        Self {
            opts,
            s: PlainState {
                rows: usize::from(rows.max(1)),
                owner: Owner::Gap,
                screen: VecDeque::new(),
                partial: None,
                open: None,
                done: Vec::new(),
                stats: Vec::new(),
                recent: VecDeque::new(),
                alt: false,
                evicted: 0,
                esc: 0,
                csi: String::new(),
            },
        }
    }

    pub fn restore(state: &TranscriptState, opts: RenderOptions) -> Result<Self> {
        Ok(Self {
            opts,
            s: serde_json::from_str(&state.state)?,
        })
    }

    fn stats_mut(&mut self, owner: Owner) -> &mut OutputStats {
        if let Some(i) = self.s.stats.iter().position(|(o, _)| *o == owner) {
            return &mut self.s.stats[i].1;
        }
        self.s.stats.push((owner, OutputStats::default()));
        &mut self.s.stats.last_mut().expect("just pushed").1
    }

    fn wanted(&self, owner: Owner) -> bool {
        self.opts.output && (owner != Owner::Gap || self.opts.gap_output)
    }

    fn close_open(&mut self) {
        if let Some(o) = self.s.open.take()
            && !o.text.trim().is_empty()
        {
            self.s.done.push(Chunk {
                kind: if o.owner == Owner::Gap {
                    ChunkKind::Gap
                } else {
                    ChunkKind::Primary
                },
                cmd_seq: o.owner.cmd_seq(),
                t: o.at.t,
                ts: o.at.ts,
                line0: o.line0,
                text: o.text,
            });
        }
    }

    /// A completed line leaves the screen into the owner's transcript.
    fn commit_line(&mut self, line: Line) {
        let owner = self.s.owner;
        if !self.wanted(owner) {
            return;
        }
        let max = self.opts.max_output_per_command;
        let st = self.stats_mut(owner);
        if st.truncated {
            return;
        }
        if owner != Owner::Gap && st.bytes + line.text.len() as u64 > max {
            st.truncated = true;
            return;
        }
        st.bytes += line.text.len() as u64;
        let line0 = st.lines;
        st.lines += 1;
        let o = self.s.open.get_or_insert_with(|| OpenChunk {
            owner,
            line0,
            at: line.at,
            text: String::new(),
            lines: 0,
        });
        if o.lines > 0 {
            o.text.push('\n');
        }
        o.text.push_str(line.text.trim_end());
        o.lines += 1;
        if o.text.len() >= self.opts.chunk_bytes || o.lines >= self.opts.chunk_lines {
            self.close_open();
        }
    }

    fn end_line(&mut self, at: Stamp) {
        let line = self.s.partial.take().unwrap_or(Line {
            text: String::new(),
            at,
        });
        self.s.recent.push_back(line.text.clone());
        if self.s.recent.len() > 64 {
            self.s.recent.pop_front();
        }
        self.s.screen.push_back(line);
        while self.s.screen.len() >= self.s.rows {
            let l = self.s.screen.pop_front().expect("non-empty");
            self.s.evicted += 1;
            self.commit_line(l);
        }
    }

    fn flush_screen(&mut self) {
        while let Some(l) = self.s.screen.pop_front() {
            self.commit_line(l);
        }
        self.close_open();
    }

    fn csi_done(&mut self) {
        let seq = std::mem::take(&mut self.s.csi);
        match seq.as_str() {
            "?1049h" | "?1047h" | "?47h" => self.s.alt = true,
            "?1049l" | "?1047l" | "?47l" => self.s.alt = false,
            _ => {}
        }
    }
}

impl Transcript for PlainTranscript {
    fn output(&mut self, at: Stamp, data: &str) {
        for c in data.chars() {
            match (self.s.esc, c) {
                (0, '\x1b') => self.s.esc = 1,
                (1, '[') => self.s.esc = 2,
                (1, ']' | 'P' | '_' | '^') => self.s.esc = 3,
                (1, _) => self.s.esc = 0,
                (2, c) if ('\x40'..='\x7e').contains(&c) => {
                    self.s.esc = 0;
                    self.s.csi.push(c);
                    self.csi_done();
                }
                (2, c) => self.s.csi.push(c),
                (3, '\x07') => self.s.esc = 0,
                (3, '\x1b') => self.s.esc = 1,
                (3, _) => {}
                (_, '\n') if !self.s.alt => self.end_line(at),
                (_, '\r' | '\n') => {}
                (_, c) if c.is_control() => {}
                (_, c) if !self.s.alt => self
                    .s
                    .partial
                    .get_or_insert(Line {
                        text: String::new(),
                        at,
                    })
                    .text
                    .push(c),
                _ => {}
            }
        }
    }

    fn resize(&mut self, _at: Stamp, _cols: u16, rows: u16) {
        self.close_open();
        self.s.rows = usize::from(rows.max(1));
    }

    fn boundary(&mut self, _at: Stamp, next: Owner) {
        self.flush_screen();
        self.s.owner = next;
    }

    fn owner(&self) -> Owner {
        self.s.owner
    }

    fn lines_above_cursor(&self, max_lines: usize) -> Vec<String> {
        let n = self.s.recent.len();
        self.s
            .recent
            .iter()
            .skip(n.saturating_sub(max_lines))
            .cloned()
            .collect()
    }

    fn is_alt_screen(&self) -> bool {
        self.s.alt
    }

    fn take_chunks(&mut self) -> Vec<Chunk> {
        std::mem::take(&mut self.s.done)
    }

    fn provisional(&self) -> Vec<Chunk> {
        let owner = self.s.owner;
        if !self.wanted(owner) {
            return Vec::new();
        }
        let mut text = self.s.open.as_ref().map(|o| o.text.clone()).unwrap_or_default();
        let mut first = self.s.open.as_ref().map(|o| o.at);
        for l in self.s.screen.iter().chain(self.s.partial.iter()) {
            first.get_or_insert(l.at);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(l.text.trim_end());
        }
        let Some(at) = first.filter(|_| !text.trim().is_empty()) else {
            return Vec::new();
        };
        let lines = self
            .s
            .stats
            .iter()
            .find(|(o, _)| *o == owner)
            .map_or(0, |(_, s)| s.lines);
        vec![Chunk {
            kind: ChunkKind::Provisional,
            cmd_seq: owner.cmd_seq(),
            t: at.t,
            ts: at.ts,
            line0: self.s.open.as_ref().map_or(lines, |o| o.line0),
            text,
        }]
    }

    fn finish(&mut self, at: Stamp) -> Vec<Chunk> {
        if self.s.partial.is_some() {
            self.end_line(at);
        }
        self.flush_screen();
        self.take_chunks()
    }

    fn output_stats(&self, owner: Owner) -> OutputStats {
        self.s
            .stats
            .iter()
            .find(|(o, _)| *o == owner)
            .map(|(_, s)| *s)
            .unwrap_or_default()
    }

    fn snapshot(&self) -> TranscriptState {
        TranscriptState {
            vt_dump: String::new(),
            evicted: self.s.evicted,
            state: serde_json::to_string(&self.s).expect("plain state serialises"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: f64) -> Stamp {
        Stamp { t, ts: 1000.0 + t }
    }

    fn session(uuid: &str) -> SessionMeta {
        SessionMeta {
            uuid: uuid.into(),
            host: "h".into(),
            scope: cinelog_proto::Scope::Personal,
            user: "u".into(),
            euid: Some(1000),
            sudo_user: None,
            claimed_user: None,
            attribution: None,
            login_via: None,
            tty: None,
            holder_comm: None,
            tmux: TmuxLocation::default(),
            parent_uuid: None,
            cast_path: format!("/nonexistent/{uuid}.cast").into(),
            parts: 1,
            compressed: false,
            cast_bytes: None,
            started_at: 1000.0,
            ended_at: None,
            exit_status: None,
            tz: None,
            cols: 80,
            rows: 24,
            term: None,
            legacy: false,
            hidden: false,
            state: SessionState::Live,
            counters: SessionCounters::default(),
        }
    }

    fn command(seq: Seq, typed: &str, t: f64) -> Command {
        Command {
            seq,
            parent_seq: None,
            depth: 0,
            shell_pid: Some(1),
            t_start: t,
            t_end: Some(t + 1.0),
            ts_start: 1000.0 + t,
            ts_end: Some(1001.0 + t),
            exit: Some(0),
            pipestatus: vec![0],
            cwd: Some("/tmp".into()),
            user: Some("u".into()),
            euid: Some(1000),
            typed: typed.into(),
            expanded: None,
            typed_trunc: false,
            approx: false,
            confidence: None,
            out_bytes: 0,
            out_truncated: false,
        }
    }

    #[test]
    fn plain_transcript_attributes_rows() {
        let opts = RenderOptions::default();
        let mut tr = PlainTranscript::new(80, 3, opts);
        tr.output(at(0.0), "$ echo hi\r\n");
        tr.boundary(at(0.1), Owner::Command(1));
        tr.output(at(0.2), "hi\r\n\x1b[1mbold\x1b[0m\r\n");
        assert_eq!(tr.provisional()[0].text, "hi\nbold");
        let snap = tr.snapshot();
        let mut tr2 = PlainTranscript::restore(&snap, opts).unwrap();
        for t in [&mut tr, &mut tr2] {
            t.boundary(at(0.3), Owner::Gap);
            let chunks = t.finish(at(1.0));
            let texts: Vec<_> = chunks.iter().map(|c| (c.cmd_seq, c.text.as_str())).collect();
            assert_eq!(texts, [(None, "$ echo hi"), (Some(1), "hi\nbold")]);
        }
        assert_eq!(tr.output_stats(Owner::Command(1)).lines, 2);
        assert_eq!(tr.lines_above_cursor(2), ["hi", "bold"]);
    }

    #[test]
    fn mem_index_search_forget_and_meta() {
        let mut w = MemIndex::new();
        let r = w.clone();
        let mut b = IndexBatch::new(session("s1"));
        b.commands = vec![command(1, "grep needle file", 1.0), command(2, "ls", 5.0)];
        b.chunks = vec![Chunk {
            kind: ChunkKind::Primary,
            cmd_seq: Some(2),
            t: 5.5,
            ts: 1005.5,
            line0: 0,
            text: "a NEEDLE in the output".into(),
        }];
        w.apply(&b).unwrap();

        let mut q = SearchQuery::new("needle");
        assert_eq!(r.search(&q).unwrap().hits.len(), 1);
        q.target = SearchTarget::All;
        let page = r.search(&q).unwrap();
        assert_eq!(page.hits.len(), 2);
        assert_eq!(page.hits[0].kind, HitKind::Output, "newest first");
        assert_eq!(
            page.hits[0].snippet.as_deref(),
            Some("a \u{E000}NEEDLE\u{E001} in the output")
        );
        q.limit = 1;
        let p1 = r.search(&q).unwrap();
        q.cursor = p1.next;
        let p2 = r.search(&q).unwrap();
        assert_eq!(p2.hits[0].cmd_seq, Some(1));
        assert!(p2.next.is_none());

        let detail = r
            .show(
                &EntryRef::Command {
                    uuid: "s1".into(),
                    seq: 2,
                },
                5,
            )
            .unwrap()
            .unwrap();
        assert_eq!((detail.output.as_str(), detail.output_truncated), ("a NEE", true));

        let mut meta = w.clone();
        let f = meta
            .forget(
                &ForgetTarget::Command {
                    uuid: "s1".into(),
                    seq: 2,
                },
                "me",
            )
            .unwrap();
        let out = w.apply_forgotten(&[f]).unwrap();
        assert_eq!((out.commands, out.outputs), (1, 1));
        w.apply(&b).unwrap();
        assert_eq!(
            r.search(&q).unwrap().hits.len(),
            1,
            "re-applied batch keeps it forgotten"
        );

        meta.set_setting(settings_keys::PORT, Some("10123")).unwrap();
        let mut buf = Vec::new();
        assert_eq!(meta.export_jsonl(&mut buf).unwrap(), 2);
        let mut other = MemIndex::new();
        assert_eq!(other.import_jsonl(&mut buf.as_slice()).unwrap(), 2);
        assert_eq!(other.setting(settings_keys::PORT).unwrap().as_deref(), Some("10123"));
        assert_eq!(other.forgotten().unwrap().len(), 1);
    }
}
