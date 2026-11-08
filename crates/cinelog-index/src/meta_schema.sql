-- meta.db, schema version 1 (plan §6.3). Authoritative: never dropped by `index --rebuild`,
-- backed up nightly as meta-YYYYMMDD.jsonl.zst. Keyed by session uuid and command seq so it
-- survives an index rebuild. `PRAGMA user_version` is set by `schema::migrate`.

CREATE TABLE annotations(
  id INTEGER PRIMARY KEY,
  uuid TEXT NOT NULL UNIQUE,
  session_uuid TEXT NOT NULL,
  cmd_seq INT,
  t_from REAL,
  t_to REAL,
  author TEXT NOT NULL,
  body TEXT,
  created_at REAL NOT NULL,
  updated_at REAL NOT NULL,
  deleted_at REAL);
CREATE INDEX annotations_session ON annotations(session_uuid, cmd_seq);

CREATE TABLE tags(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, color TEXT);

CREATE TABLE annotation_tags(
  annotation_id INT NOT NULL REFERENCES annotations ON DELETE CASCADE,
  tag_id INT NOT NULL REFERENCES tags ON DELETE CASCADE,
  PRIMARY KEY(annotation_id, tag_id));

CREATE TABLE auth_sessions(
  id_hash BLOB PRIMARY KEY,
  principal TEXT NOT NULL,
  client_ip TEXT,
  ua_hash BLOB,
  created_at REAL NOT NULL,
  last_seen REAL NOT NULL,
  expires_at REAL NOT NULL);

-- `cinelog forget` (§7.6, §15.7): excluded at query time and deleted from index.db by the
-- writer. kind 'command' uses cmd_seq; 'range' uses t_from/t_to; 'session' uses neither.
CREATE TABLE forgotten(
  id INTEGER PRIMARY KEY,
  session_uuid TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN('session','command','range')),
  cmd_seq INT,
  t_from REAL,
  t_to REAL,
  by TEXT NOT NULL,
  at REAL NOT NULL,
  CHECK(kind <> 'command' OR cmd_seq IS NOT NULL),
  CHECK(kind <> 'range' OR (t_from IS NOT NULL AND t_to IS NOT NULL AND t_from <= t_to)));
CREATE UNIQUE INDEX forgotten_target ON forgotten(
  session_uuid, kind, ifnull(cmd_seq, -1), ifnull(t_from, -1), ifnull(t_to, -1));

-- Persisted server facts (keys: model::settings_keys).
CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT);
