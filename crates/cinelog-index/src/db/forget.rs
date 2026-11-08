//! Matching rows against `meta.forgotten` (§7.6). Shared by the writer (drops, blanks and
//! deletes rows) and the reader (excludes rows the writer has not purged yet).
//!
//! - `session`: everything of the session;
//! - `command`: the command and all its output;
//! - `range` (inclusive, session-relative t): commands that start and end inside it (with
//!   their output) and every output chunk whose time span overlaps it.
//!
//! A chunk's span runs from its `t` (first write of its first line) to its *end*: the `t` of
//! the next chunk of the same owner (a command, or the session's unattributed output) and
//! kind, capped by the command's `t_end`. Without either the end is open, and a chunk that
//! starts before a range and may still run into it counts as forgotten.
//!
//! The writer *blanks* range-forgotten chunks (empty text, so they leave the FTS index) rather
//! than deleting them: the row keeps the span of the chunk before it intact, so purging is
//! idempotent and never makes an earlier chunk look like it reaches into the range. It blanks
//! a chunk that starts before a range once its end is known (or the session is over).
//!
//! A command forget also hides the command's [`Echo`]: the shell prints the command line
//! before its `cmd` marker, so the echo belongs to the output before the command (the
//! session's unattributed output, or the enclosing command's). The writer replaces the echoed
//! lines with empty ones (line ordinals stay) and the reader strips them before the purge.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::U64;
use crate::error::Result;
use crate::model::{ChunkKind, ForgetTarget, Forgotten, Seq};

#[derive(Debug, Default, Clone)]
struct Entry {
    session: bool,
    commands: HashSet<Seq>,
    ranges: Vec<(f64, f64)>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ForgetSet {
    by_uuid: HashMap<String, Entry>,
}

impl ForgetSet {
    pub(crate) fn new(list: &[Forgotten]) -> Self {
        let mut by_uuid: HashMap<String, Entry> = HashMap::new();
        for f in list {
            let e = by_uuid.entry(f.target.uuid().to_owned()).or_default();
            match &f.target {
                ForgetTarget::Session { .. } => e.session = true,
                ForgetTarget::Command { seq, .. } => {
                    e.commands.insert(*seq);
                }
                ForgetTarget::Range { t_from, t_to, .. } => e.ranges.push((*t_from, *t_to)),
            }
        }
        Self { by_uuid }
    }

    pub(crate) fn session(&self, uuid: &str) -> bool {
        self.by_uuid.get(uuid).is_some_and(|e| e.session)
    }

    /// Whether command `seq` is forgotten by its own `command` row.
    pub(crate) fn by_seq(&self, uuid: &str, seq: Seq) -> bool {
        self.by_uuid.get(uuid).is_some_and(|e| e.commands.contains(&seq))
    }

    /// The session's commands forgotten by seq, in ascending order.
    pub(crate) fn seqs(&self, uuid: &str) -> Vec<Seq> {
        let mut v: Vec<Seq> = self
            .by_uuid
            .get(uuid)
            .map(|e| e.commands.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// The session's forgotten ranges.
    pub(crate) fn ranges(&self, uuid: &str) -> &[(f64, f64)] {
        self.by_uuid.get(uuid).map_or(&[], |e| &e.ranges)
    }

    pub(crate) fn command(&self, uuid: &str, seq: Seq, t_start: f64, t_end: Option<f64>) -> bool {
        self.by_uuid.get(uuid).is_some_and(|e| {
            e.session
                || e.commands.contains(&seq)
                || t_end.is_some_and(|end| e.ranges.iter().any(|&(a, b)| t_start >= a && end <= b))
        })
    }

    /// Whether a chunk's owner is forgotten as a whole (its session or its command by seq).
    pub(crate) fn owner(&self, uuid: &str, cmd_seq: Option<Seq>) -> bool {
        self.by_uuid
            .get(uuid)
            .is_some_and(|e| e.session || cmd_seq.is_some_and(|s| e.commands.contains(&s)))
    }

    /// Whether `t` lies inside a forgotten range of the session.
    pub(crate) fn in_range(&self, uuid: &str, t: f64) -> bool {
        self.ranges(uuid).iter().any(|&(a, b)| t >= a && t <= b)
    }

    /// Whether the chunk starting at `t` is forgotten. For a chunk that starts before a range,
    /// `runs_past(a)` tells whether its span reaches past `a`, the start of the nearest range
    /// after `t` (it then overlaps that range).
    pub(crate) fn chunk(
        &self,
        uuid: &str,
        cmd_seq: Option<Seq>,
        t: f64,
        runs_past: impl FnOnce(f64) -> Result<bool>,
    ) -> Result<bool> {
        if self.owner(uuid, cmd_seq) || self.in_range(uuid, t) {
            return Ok(true);
        }
        match self
            .ranges(uuid)
            .iter()
            .map(|&(a, _)| a)
            .filter(|&a| t < a)
            .reduce(f64::min)
        {
            Some(a) => runs_past(a),
            None => Ok(false),
        }
    }
}

/// Slack for comparing chunk times with command times (both come from the same µs clock).
const T_EPS: f64 = 1e-6;

/// Where a command's echo can be and what it looks like (module docs).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Echo {
    /// Row of the command in index.db, if it has one.
    command_id: Option<i64>,
    /// The end of the command before it, the start of an enclosing one, or the session
    /// start, lowered to the start of the echo-owning chunk that covers it.
    from: f64,
    /// The command's start.
    to: f64,
    /// Non-blank lines of the typed command, trimmed.
    lines: Vec<String>,
}

impl Echo {
    /// The echo of command `seq` of session `sid`, typed as `typed` and started at
    /// `t_start`; `command_id` is its row, if stored.
    pub(crate) fn new(
        conn: &Connection,
        sid: i64,
        command_id: Option<i64>,
        seq: Seq,
        t_start: f64,
        typed: &str,
    ) -> Result<Self> {
        let from: Option<f64> = conn
            .prepare_cached(
                "SELECT max(CASE WHEN t_end IS NOT NULL AND t_end <= ?3 THEN t_end ELSE t_start END)
                 FROM commands WHERE session_id = ?1 AND seq <> ?2 AND t_start < ?3",
            )?
            .query_row(params![sid, U64(seq), t_start], |r| r.get(0))?;
        let from = from.unwrap_or(0.0).min(t_start);
        // The shell may write the prompt (and so the echo) just before the previous command's
        // end is stamped: the chunk that covers `from` is part of the echo's window too.
        let covering: Option<f64> = conn
            .prepare_cached(&format!(
                "SELECT max(o.t) FROM outputs o LEFT JOIN commands c ON c.id = o.command_id
                 WHERE o.session_id = ?1 AND o.t <= ?2 AND {ECHO_OWNER}"
            ))?
            .query_row(params![sid, from + T_EPS, t_start, command_id], |r| r.get(0))?;
        let lines = typed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self {
            command_id,
            from: covering.map_or(from, |c| c.min(from)),
            to: t_start,
            lines,
        })
    }

    /// The echo of stored command `seq` of session `sid`; `None` once its row is gone.
    pub(crate) fn of_stored(conn: &Connection, sid: i64, seq: Seq) -> Result<Option<Self>> {
        let row: Option<(i64, f64, String)> = conn
            .prepare_cached("SELECT id, t_start, typed FROM commands WHERE session_id = ?1 AND seq = ?2")?
            .query_row(params![sid, U64(seq)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        row.map(|(id, t, typed)| Self::new(conn, sid, Some(id), seq, t, &typed))
            .transpose()
    }

    /// Whether a chunk starting at `t` can hold the echo by its time. Its owner must be
    /// unattributed or open at the command's start ([`ECHO_OWNER`]).
    pub(crate) fn spans(&self, t: f64) -> bool {
        t >= self.from - T_EPS && t <= self.to + T_EPS
    }

    /// `text` with every line that shows a line of the command replaced by an empty one,
    /// or `None` when there is none. A chunk left without text becomes empty.
    pub(crate) fn strip(&self, text: &str) -> Option<String> {
        if self.lines.is_empty() {
            return None;
        }
        let mut hit = false;
        let lines: Vec<&str> = text
            .split('\n')
            .map(|l| {
                if self.lines.iter().any(|e| l.contains(e.as_str())) {
                    hit = true;
                    ""
                } else {
                    l
                }
            })
            .collect();
        if !hit {
            return None;
        }
        let out = lines.join("\n");
        Some(if out.trim().is_empty() { String::new() } else { out })
    }

    /// Replaces the echoed lines in the stored chunks of session `sid`; returns how many
    /// chunks changed.
    pub(crate) fn purge(&self, conn: &Connection, sid: i64) -> Result<u64> {
        if self.lines.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "SELECT o.id, o.text FROM outputs o LEFT JOIN commands c ON c.id = o.command_id
             WHERE o.session_id = ?1 AND o.t >= ?2 AND o.t <= ?3 AND o.text <> '' AND {ECHO_OWNER}"
        );
        let found: Vec<(i64, String)> = conn
            .prepare_cached(&sql)?
            .query_map(params![sid, self.from - T_EPS, self.to + T_EPS, self.command_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut changed = 0;
        for (id, text) in found {
            if let Some(new) = self.strip(&text) {
                conn.prepare_cached("UPDATE outputs SET text = ?2 WHERE id = ?1")?
                    .execute(params![id, new])?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Whether chunks of `kind` can hold an echo.
    pub(crate) fn kind(kind: ChunkKind) -> bool {
        matches!(kind, ChunkKind::Primary | ChunkKind::Gap)
    }

    /// Whether stored chunk `id` has an owner that can hold the echo.
    pub(crate) fn owner_of(&self, conn: &Connection, id: i64) -> Result<bool> {
        let sql = format!(
            "SELECT 1 FROM outputs o LEFT JOIN commands c ON c.id = o.command_id WHERE o.id = ?1 AND {ECHO_OWNER}"
        );
        Ok(conn
            .prepare_cached(&sql)?
            .query_row(
                params![id, Option::<i64>::None, self.to + T_EPS, self.command_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

/// The owners of an echo, over `outputs o LEFT JOIN commands c ON c.id = o.command_id` with
/// `?3` the command's start and `?4` its row: primary or unattributed output, unattributed or
/// of another command still running when it started (an enclosing one).
const ECHO_OWNER: &str = "o.kind IN ('primary', 'gap') AND (o.command_id IS NULL
      OR (o.command_id IS NOT ?4 AND c.t_start < ?3 AND (c.t_end IS NULL OR c.t_end >= ?3)))";

/// Whether stored chunk `id` runs past `a` (module docs): no later chunk of its owner and kind
/// starts by `a`, and its command (if any) had not ended by `a`. A chunk that no longer
/// exists does not.
pub(crate) fn chunk_runs_past(conn: &Connection, id: i64, a: f64) -> Result<bool> {
    /// session_id, command_id, kind, t, the command's t_end.
    type ChunkRow = (i64, Option<i64>, String, f64, Option<f64>);
    let row: Option<ChunkRow> = conn
        .prepare_cached(
            "SELECT o.session_id, o.command_id, o.kind, o.t, (SELECT c.t_end FROM commands c WHERE c.id = o.command_id)
             FROM outputs o WHERE o.id = ?1",
        )?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .optional()?;
    let Some((sid, cmd, kind, t, t_end)) = row else {
        return Ok(false);
    };
    if t_end.is_some_and(|e| e <= a) {
        return Ok(false);
    }
    // A short walk of outputs_sess over [t, a].
    let next_by_a = conn
        .prepare_cached(
            "SELECT 1 FROM outputs WHERE session_id = ?1 AND t >= ?2 AND t <= ?3 AND (t > ?2 OR id > ?4)
               AND command_id IS ?5 AND kind = ?6 LIMIT 1",
        )?
        .query_row(params![sid, t, a, id, cmd, kind], |_| Ok(()))
        .optional()?;
    Ok(next_by_a.is_none())
}

/// Blanks the chunks of session `?1` that start before `?2` (a range's `t_from`) and run past
/// it, once that is known: per owner and kind only the last chunk starting before `?2` can.
/// It does when no chunk of its group starts at `?2` and its command ended after `?2`, or the
/// command is open (unattributed output always is) and a later chunk of the group exists or
/// the session is over.
pub(crate) const BLANK_STRADDLING: &str = "UPDATE outputs SET text = '' WHERE text <> '' AND id IN (
    SELECT l.id FROM (
      SELECT o.id, o.t, o.command_id, o.kind,
        row_number() OVER (PARTITION BY o.command_id, o.kind ORDER BY o.t DESC, o.id DESC) AS rn
      FROM outputs o WHERE o.session_id = ?1 AND o.t <= ?2) l
    LEFT JOIN commands c ON c.id = l.command_id
    WHERE l.rn = 1 AND l.t < ?2 AND (c.t_end > ?2 OR (c.t_end IS NULL AND (
      (SELECT state FROM sessions WHERE id = ?1) <> 'live'
      OR EXISTS (SELECT 1 FROM outputs n WHERE n.session_id = ?1 AND n.t > ?2 AND n.command_id IS l.command_id
                 AND n.kind = l.kind)))))";

/// Blanks the chunks of session `?1` that start inside [`?2`, `?3`].
pub(crate) const BLANK_INSIDE: &str =
    "UPDATE outputs SET text = '' WHERE session_id = ?1 AND t >= ?2 AND t <= ?3 AND text <> ''";

#[cfg(test)]
mod tests {
    use super::*;

    fn f(target: ForgetTarget) -> Forgotten {
        Forgotten {
            target,
            by: "me".into(),
            at: 0.0,
        }
    }

    fn chunk(set: &ForgetSet, uuid: &str, seq: Option<Seq>, t: f64, past: bool) -> bool {
        set.chunk(uuid, seq, t, |_| Ok(past)).unwrap()
    }

    #[test]
    fn targets() {
        let set = ForgetSet::new(&[
            f(ForgetTarget::Session { uuid: "a".into() }),
            f(ForgetTarget::Command {
                uuid: "b".into(),
                seq: 3,
            }),
            f(ForgetTarget::Range {
                uuid: "b".into(),
                t_from: 10.0,
                t_to: 20.0,
            }),
        ]);
        assert!(set.session("a") && !set.session("b") && !set.session("c"));
        assert_eq!(set.ranges("b"), &[(10.0, 20.0)]);
        assert!(set.ranges("a").is_empty());
        assert!(set.command("a", 1, 0.0, None));
        assert!(set.command("b", 3, 0.0, None));
        assert!(!set.command("b", 4, 0.0, Some(1.0)));
        assert!(set.command("b", 4, 10.0, Some(20.0)));
        assert!(!set.command("b", 4, 10.0, Some(20.5)), "only partly inside");
        assert!(!set.command("b", 4, 10.0, None), "still running");
        assert!(set.owner("a", None) && set.owner("b", Some(3)) && !set.owner("b", Some(4)));
        assert!(set.in_range("b", 10.0) && set.in_range("b", 20.0) && !set.in_range("b", 20.1));
        assert!(chunk(&set, "b", Some(3), 99.0, false));
        assert!(chunk(&set, "b", None, 15.0, false));
        assert!(chunk(&set, "b", Some(4), 20.0, false));
        assert!(!chunk(&set, "b", None, 20.1, true), "after the range");
        assert!(!chunk(&set, "c", None, 15.0, true));
        assert!(
            !ForgetSet::new(&[])
                .chunk("a", None, 0.0, |_| panic!("not needed"))
                .unwrap()
        );
    }

    #[test]
    fn echo_lines_are_emptied() {
        let echo = Echo {
            command_id: None,
            from: 5.0,
            to: 9.0,
            lines: vec!["for f in *; do".into(), "echo $f".into(), "done".into()],
        };
        assert!(echo.spans(5.0) && echo.spans(9.0) && !echo.spans(4.9) && !echo.spans(9.1));
        let text = "motd\n$ for f in *; do\n> echo $f\n> done    12:00\n";
        assert_eq!(echo.strip(text).as_deref(), Some("motd\n\n\n\n"));
        assert_eq!(echo.strip("motd"), None);
        assert_eq!(
            echo.strip("$ for f in *; do\n> done").as_deref(),
            Some(""),
            "nothing left"
        );
        let none = Echo {
            lines: Vec::new(),
            ..echo
        };
        assert_eq!(none.strip("anything"), None);
    }

    /// A chunk that starts before a range is forgotten when its span reaches past the start
    /// of the nearest range after it.
    #[test]
    fn straddling_chunks() {
        let set = ForgetSet::new(&[
            f(ForgetTarget::Range {
                uuid: "b".into(),
                t_from: 30.0,
                t_to: 40.0,
            }),
            f(ForgetTarget::Range {
                uuid: "b".into(),
                t_from: 10.0,
                t_to: 20.0,
            }),
        ]);
        let asked = |t: f64| {
            let mut seen = None;
            let _ = set.chunk("b", Some(1), t, |a| {
                seen = Some(a);
                Ok(true)
            });
            seen
        };
        assert_eq!(asked(5.0), Some(10.0), "the nearest range");
        assert_eq!(asked(25.0), Some(30.0));
        assert_eq!(asked(45.0), None, "after every range");
        assert!(chunk(&set, "b", Some(1), 5.0, true));
        assert!(!chunk(&set, "b", Some(1), 5.0, false));
    }
}
