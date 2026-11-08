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

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::model::{ForgetTarget, Forgotten, Seq};

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
