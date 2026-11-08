//! meta.db (§6.3): forget requests, settings, annotations and tags, and the JSON Lines
//! export/import behind the nightly backup and `cinelog meta export|import`.

use std::io::{BufRead, Write};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::U64;
use crate::api::MetaStore;
use crate::error::{IndexError, Result};
use crate::model::{Annotation, AnnotationQuery, ForgetTarget, Forgotten, TagInfo};
use crate::schema::{self, Access, DbKind};

pub(crate) struct SqliteMeta {
    conn: Connection,
}

impl SqliteMeta {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            conn: schema::open(path, DbKind::Meta, Access::ReadWrite)?,
        })
    }
}

const FORGOTTEN_SELECT: &str = "SELECT session_uuid, kind, cmd_seq, t_from, t_to, by, at FROM forgotten";

fn forgotten_from_row(r: &Row<'_>) -> rusqlite::Result<Forgotten> {
    let uuid: String = r.get(0)?;
    let kind: String = r.get(1)?;
    let target = match kind.as_str() {
        "session" => ForgetTarget::Session { uuid },
        "command" => ForgetTarget::Command {
            uuid,
            seq: super::u64_at(r, 2)?,
        },
        "range" => ForgetTarget::Range {
            uuid,
            t_from: r.get(3)?,
            t_to: r.get(4)?,
        },
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                format!("forgotten kind {other:?}").into(),
            ));
        }
    };
    Ok(Forgotten {
        target,
        by: r.get(5)?,
        at: r.get(6)?,
    })
}

/// Every `forgotten` row in insertion order.
pub(crate) fn read_forgotten(conn: &Connection) -> Result<Vec<Forgotten>> {
    let mut st = conn.prepare_cached(&format!("{FORGOTTEN_SELECT} ORDER BY id"))?;
    let rows = st.query_map([], forgotten_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn target_columns(t: &ForgetTarget) -> (Option<U64>, Option<f64>, Option<f64>) {
    match *t {
        ForgetTarget::Session { .. } => (None, None, None),
        ForgetTarget::Command { seq, .. } => (Some(U64(seq)), None, None),
        ForgetTarget::Range { t_from, t_to, .. } => (None, Some(t_from), Some(t_to)),
    }
}

fn validate(t: &ForgetTarget) -> Result<()> {
    if let ForgetTarget::Range { t_from, t_to, .. } = *t
        && !(t_from.is_finite() && t_to.is_finite() && t_from <= t_to)
    {
        return Err(IndexError::Invalid(format!("forget range {t_from}..{t_to}")));
    }
    if t.uuid().is_empty() {
        return Err(IndexError::Invalid("forget target without session uuid".into()));
    }
    Ok(())
}

/// Inserts unless the same target exists; returns the stored row either way.
fn insert_forgotten(conn: &Connection, f: &Forgotten) -> Result<Forgotten> {
    validate(&f.target)?;
    let (seq, from, to) = target_columns(&f.target);
    conn.prepare_cached(
        "INSERT INTO forgotten(session_uuid, kind, cmd_seq, t_from, t_to, by, at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
    )?
    .execute(params![f.target.uuid(), f.target.kind(), seq, from, to, f.by, f.at])?;
    let existing = conn
        .prepare_cached(&format!(
            "{FORGOTTEN_SELECT} WHERE session_uuid = ?1 AND kind = ?2 AND ifnull(cmd_seq, -1) = ifnull(?3, -1)
             AND ifnull(t_from, -1) = ifnull(?4, -1) AND ifnull(t_to, -1) = ifnull(?5, -1)"
        ))?
        .query_row(
            params![f.target.uuid(), f.target.kind(), seq, from, to],
            forgotten_from_row,
        )?;
    Ok(existing)
}

fn tag_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.prepare_cached("INSERT INTO tags(name) VALUES(?1) ON CONFLICT(name) DO NOTHING")?
        .execute([name])?;
    Ok(conn
        .prepare_cached("SELECT id FROM tags WHERE name = ?1")?
        .query_row([name], |r| r.get(0))?)
}

/// Upserts an annotation by uuid and replaces its tags. With `only_if_newer`, an existing
/// row with a later `updated_at` wins (import merge). Returns whether the row was written.
fn upsert_annotation(conn: &Connection, a: &Annotation, only_if_newer: bool) -> Result<bool> {
    let sql = format!(
        "INSERT INTO annotations(uuid, session_uuid, cmd_seq, t_from, t_to, author, body, created_at, updated_at, deleted_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(uuid) DO UPDATE SET session_uuid = excluded.session_uuid, cmd_seq = excluded.cmd_seq,
           t_from = excluded.t_from, t_to = excluded.t_to, author = excluded.author, body = excluded.body,
           created_at = excluded.created_at, updated_at = excluded.updated_at, deleted_at = excluded.deleted_at
         {}
         RETURNING id",
        if only_if_newer {
            "WHERE excluded.updated_at >= annotations.updated_at"
        } else {
            ""
        }
    );
    let id: Option<i64> = conn
        .prepare_cached(&sql)?
        .query_row(
            params![
                a.uuid,
                a.session_uuid,
                super::opt_u64(a.cmd_seq),
                a.t_from,
                a.t_to,
                a.author,
                a.body,
                a.created_at,
                a.updated_at,
                a.deleted_at
            ],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(false) };
    conn.prepare_cached("DELETE FROM annotation_tags WHERE annotation_id = ?1")?
        .execute([id])?;
    for name in &a.tags {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let tag = tag_id(conn, name)?;
        conn.prepare_cached(
            "INSERT INTO annotation_tags(annotation_id, tag_id) VALUES(?1, ?2) ON CONFLICT DO NOTHING",
        )?
        .execute([id, tag])?;
    }
    Ok(true)
}

/// Annotations selected and ordered by `tail` (`WHERE … ORDER BY …`).
fn read_annotations(conn: &Connection, tail: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Annotation>> {
    let mut st = conn.prepare_cached(&format!(
        "SELECT id, uuid, session_uuid, cmd_seq, t_from, t_to, author, body, created_at, updated_at, deleted_at
         FROM annotations {tail}"
    ))?;
    let rows = st.query_map(args, |r| {
        Ok((
            r.get::<_, i64>(0)?,
            Annotation {
                uuid: r.get(1)?,
                session_uuid: r.get(2)?,
                cmd_seq: super::opt_u64_at(r, 3)?,
                t_from: r.get(4)?,
                t_to: r.get(5)?,
                author: r.get(6)?,
                body: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                tags: Vec::new(),
                created_at: r.get(8)?,
                updated_at: r.get(9)?,
                deleted_at: r.get(10)?,
            },
        ))
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tags = conn.prepare_cached(
        "SELECT t.name FROM annotation_tags a JOIN tags t ON t.id = a.tag_id WHERE a.annotation_id = ?1 ORDER BY t.name",
    )?;
    for (id, a) in &mut out {
        a.tags = tags.query_map([*id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    }
    Ok(out.into_iter().map(|(_, a)| a).collect())
}

/// Tags by name with their live usage count; `only` selects one name (empty: all).
fn read_tags(conn: &Connection, only: &str) -> Result<Vec<TagInfo>> {
    let mut st = conn.prepare_cached(
        "SELECT t.name, t.color,
           (SELECT count(*) FROM annotation_tags x JOIN annotations a ON a.id = x.annotation_id
            WHERE x.tag_id = t.id AND a.deleted_at IS NULL)
         FROM tags t WHERE ?1 = '' OR t.name = ?1 ORDER BY t.name",
    )?;
    let rows = st.query_map([only], |r| {
        Ok(TagInfo {
            name: r.get(0)?,
            color: r.get(1)?,
            count: super::u64_at(r, 2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A `tags` row of the export (keeps colours; membership travels inside annotations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TagRow {
    name: String,
    color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SettingRow {
    key: String,
    value: String,
}

/// One export line: `{"table": …, "row": {…}}`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "table", content = "row", rename_all = "lowercase")]
enum MetaLine {
    Tags(TagRow),
    Annotations(Annotation),
    Forgotten(Forgotten),
    Settings(SettingRow),
}

const KNOWN_TABLES: &[&str] = &["tags", "annotations", "forgotten", "settings"];

impl MetaStore for SqliteMeta {
    fn forget(&mut self, target: &ForgetTarget, by: &str) -> Result<Forgotten> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let f = insert_forgotten(
            &tx,
            &Forgotten {
                target: target.clone(),
                by: by.to_owned(),
                at: super::now(),
            },
        )?;
        tx.commit()?;
        Ok(f)
    }

    fn forgotten(&self) -> Result<Vec<Forgotten>> {
        read_forgotten(&self.conn)
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?1")?
            .query_row([key], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }

    fn set_setting(&mut self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self
                .conn
                .prepare_cached(
                    "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                )?
                .execute([key, v])?,
            None => self
                .conn
                .prepare_cached("DELETE FROM settings WHERE key = ?1")?
                .execute([key])?,
        };
        Ok(())
    }

    fn put_annotation(&mut self, a: &Annotation) -> Result<()> {
        if a.uuid.is_empty() || a.session_uuid.is_empty() {
            return Err(IndexError::Invalid("annotation without uuid or session uuid".into()));
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_annotation(&tx, a, false)?;
        tx.commit()?;
        Ok(())
    }

    fn annotations(&self, session_uuid: &str) -> Result<Vec<Annotation>> {
        read_annotations(
            &self.conn,
            "WHERE session_uuid = ?1 ORDER BY created_at, id",
            &[&session_uuid],
        )
    }

    fn annotation(&self, uuid: &str) -> Result<Option<Annotation>> {
        Ok(read_annotations(&self.conn, "WHERE uuid = ?1", &[&uuid])?.pop())
    }

    fn find_annotations(&self, q: &AnnotationQuery) -> Result<Vec<Annotation>> {
        let mut clauses = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut arg = |clause: &str, value: Box<dyn rusqlite::ToSql>| {
            args.push(value);
            clauses.push(clause.replace('?', &format!("?{}", args.len())));
        };
        if !q.sessions.is_empty() {
            arg(
                "session_uuid IN (SELECT value FROM json_each(?))",
                Box::new(serde_json::to_string(&q.sessions)?),
            );
        }
        if let Some(seq) = q.cmd_seq {
            arg("cmd_seq = ?", Box::new(U64(seq)));
        }
        if let Some(tag) = &q.tag {
            arg(
                "id IN (SELECT a.annotation_id FROM annotation_tags a JOIN tags t ON t.id = a.tag_id WHERE t.name = ?)",
                Box::new(tag.clone()),
            );
        }
        if let Some(author) = &q.author {
            arg("author = ?", Box::new(author.clone()));
        }
        if !q.include_deleted {
            clauses.push("deleted_at IS NULL".to_owned());
        }
        if let Some((created_at, uuid)) = &q.after {
            args.push(Box::new(*created_at));
            args.push(Box::new(uuid.clone()));
            let (c, u) = (args.len() - 1, args.len());
            clauses.push(format!("(created_at > ?{c} OR (created_at = ?{c} AND uuid > ?{u}))"));
        }
        let mut tail = String::new();
        if !clauses.is_empty() {
            tail = format!("WHERE {}", clauses.join(" AND "));
        }
        tail.push_str(" ORDER BY created_at, uuid");
        if q.limit > 0 {
            tail.push_str(&format!(" LIMIT {}", q.limit));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        read_annotations(&self.conn, &tail, &refs)
    }

    fn tags(&self) -> Result<Vec<TagInfo>> {
        read_tags(&self.conn, "")
    }

    fn put_tag(&mut self, name: &str, color: Option<&str>) -> Result<TagInfo> {
        let name = name.trim();
        if name.is_empty() {
            return Err(IndexError::Invalid("empty tag name".into()));
        }
        self.conn
            .prepare_cached(
                "INSERT INTO tags(name, color) VALUES(?1, ?2) ON CONFLICT(name) DO UPDATE SET color = excluded.color",
            )?
            .execute(params![name, color])?;
        read_tags(&self.conn, name)?
            .pop()
            .ok_or_else(|| IndexError::NotFound(format!("tag {name:?}")))
    }

    fn export_jsonl(&self, out: &mut dyn Write) -> Result<u64> {
        // One read transaction: a consistent snapshot even while others write.
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0u64;
        let mut emit = |line: MetaLine| -> Result<()> {
            serde_json::to_writer(&mut *out, &line)?;
            out.write_all(b"\n")?;
            n += 1;
            Ok(())
        };
        let tags: Vec<TagRow> = tx
            .prepare("SELECT name, color FROM tags ORDER BY name")?
            .query_map([], |r| {
                Ok(TagRow {
                    name: r.get(0)?,
                    color: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        tags.into_iter().map(MetaLine::Tags).try_for_each(&mut emit)?;
        read_annotations(&tx, "ORDER BY created_at, id", &[])?
            .into_iter()
            .map(MetaLine::Annotations)
            .try_for_each(&mut emit)?;
        read_forgotten(&tx)?
            .into_iter()
            .map(MetaLine::Forgotten)
            .try_for_each(&mut emit)?;
        let settings: Vec<SettingRow> = tx
            .prepare("SELECT key, value FROM settings WHERE value IS NOT NULL ORDER BY key")?
            .query_map([], |r| {
                Ok(SettingRow {
                    key: r.get(0)?,
                    value: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        settings.into_iter().map(MetaLine::Settings).try_for_each(&mut emit)?;
        out.flush()?;
        Ok(n)
    }

    fn import_jsonl(&mut self, input: &mut dyn BufRead) -> Result<u64> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut n = 0u64;
        for (lineno, line) in input.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line)?;
            let table = value.get("table").and_then(|t| t.as_str()).unwrap_or_default();
            if !KNOWN_TABLES.contains(&table) {
                tracing::warn!(line = lineno + 1, table, "meta import: skipping unknown table");
                continue;
            }
            match serde_json::from_value::<MetaLine>(value)? {
                MetaLine::Tags(t) => {
                    tx.prepare_cached(
                        "INSERT INTO tags(name, color) VALUES(?1, ?2)
                         ON CONFLICT(name) DO UPDATE SET color = ifnull(excluded.color, tags.color)",
                    )?
                    .execute(params![t.name, t.color])?;
                }
                MetaLine::Annotations(a) => {
                    upsert_annotation(&tx, &a, true)?;
                }
                MetaLine::Forgotten(f) => {
                    insert_forgotten(&tx, &f)?;
                }
                MetaLine::Settings(s) => {
                    tx.prepare_cached(
                        "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    )?
                    .execute([&s.key, &s.value])?;
                }
            }
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }
}
