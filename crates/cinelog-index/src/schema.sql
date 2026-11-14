-- index.db, schema version 1 (plan §6.2). Derived data: `cinelog index --rebuild` drops it and
-- rebuilds it from the casts. `PRAGMA user_version` is set by `schema::migrate`, not here.
--
-- Conventions (frozen for 1.x):
--   t, t_start, t_end   session-relative cast offset in seconds (part t_offset already added)
--   ts, ts_start, ts_end UTC Unix epoch in seconds (µs precision)
--   booleans            INT 0/1
--   uuid                session uuid (UUIDv7 for cinelog casts, deterministic for legacy casts)
-- Writers upsert with `INSERT … ON CONFLICT … DO UPDATE`, never `INSERT OR REPLACE`: REPLACE
-- deletes the old row without firing the FTS delete triggers.

CREATE TABLE sessions(
  id INTEGER PRIMARY KEY,
  uuid TEXT NOT NULL UNIQUE,
  host TEXT NOT NULL,
  scope TEXT NOT NULL CHECK(scope IN('team','personal')),
  user TEXT NOT NULL,
  euid INT,
  sudo_user TEXT,
  claimed_user TEXT,
  attribution TEXT,                  -- ancestor | loginuid | inherited | none | legacy
  login_via TEXT,
  tty TEXT,
  holder_comm TEXT,                  -- 'tmux: server', 'sshd-session', 'terminator', 'sudo', …
  tmux_server TEXT,                  -- '<server_pid>:<server_starttime>' (survives pane-id reuse)
  tmux_session_id TEXT,
  tmux_session TEXT,                 -- latest name (ctx markers)
  tmux_window TEXT,                  -- latest window index
  tmux_window_name TEXT,
  tmux_pane TEXT,                    -- pane id, e.g. '%3'
  parent_uuid TEXT,                  -- nested sudo / outer cast of a tmux client
  cast_path TEXT NOT NULL UNIQUE,    -- first part; '.cast.zst' once compressed
  parts INT NOT NULL DEFAULT 1,
  compressed INT NOT NULL DEFAULT 0, -- 1 when every part is compressed
  cast_bytes INT,                    -- uncompressed bytes of all parts
  started_at REAL NOT NULL,          -- ts of t=0
  ended_at REAL,
  exit_status INT,                   -- from the `x` event
  tz TEXT,
  cols INT,
  rows INT,
  term TEXT,
  legacy INT NOT NULL DEFAULT 0,
  hidden INT NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK(state IN('live','ended','crashed','degraded','error')),
  bad_lines INT NOT NULL DEFAULT 0,
  bad_markers INT NOT NULL DEFAULT 0,
  lossy_bytes INT NOT NULL DEFAULT 0, tmux_pane_index TEXT);
CREATE INDEX sessions_started ON sessions(started_at);
CREATE INDEX sessions_tree ON sessions(host, tmux_server, tmux_session_id, tmux_pane);
CREATE INDEX sessions_state ON sessions(state);

CREATE TABLE cast_parts(
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  part INT NOT NULL,                 -- 1-based
  path TEXT NOT NULL,
  compressed INT NOT NULL DEFAULT 0,
  bytes INT,                         -- uncompressed
  t_offset REAL NOT NULL DEFAULT 0,  -- session t at the part's t=0
  PRIMARY KEY(session_id, part));

-- Resume checkpoint, one per session (§6.4). Taken only at an event-line boundary.
CREATE TABLE index_state(
  session_id INTEGER PRIMARY KEY REFERENCES sessions ON DELETE CASCADE,
  part INT NOT NULL,
  byte_offset INT NOT NULL,          -- uncompressed offset of the next unread line in `part`
  t REAL NOT NULL,
  vt_dump BLOB,                      -- avt Terminal::dump() (UTF-8)
  evicted INT NOT NULL DEFAULT 0,    -- primary rows scrolled off so far
  open_stack TEXT,                   -- JSON [OpenCommand], innermost last
  state_json TEXT,                   -- pipeline-private JSON, opaque to everyone else
  file_size INT,
  file_mtime REAL,
  file_ino INT,
  updated_at REAL);

CREATE TABLE commands(
  id INTEGER PRIMARY KEY,
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  seq INT NOT NULL,                  -- recorder-assigned (legacy: importer-assigned), unique per session
  parent_seq INT,                    -- enclosing command of another shell (zsh inside zsh)
  depth INT NOT NULL DEFAULT 0,
  shell_pid INT,
  t_start REAL NOT NULL,
  t_end REAL,                        -- NULL while running, or closed without `done`
  ts_start REAL NOT NULL,
  ts_end REAL,
  exit INT,
  pipestatus TEXT,                   -- JSON array, NULL when unknown
  cwd TEXT,
  user TEXT,
  euid INT,
  typed TEXT NOT NULL,
  expanded TEXT,
  typed_trunc INT NOT NULL DEFAULT 0,
  approx INT NOT NULL DEFAULT 0,     -- legacy alignment
  confidence REAL,
  out_bytes INT NOT NULL DEFAULT 0,
  out_truncated INT NOT NULL DEFAULT 0,
  UNIQUE(session_id, seq));
CREATE INDEX commands_ts ON commands(ts_start);
CREATE INDEX commands_sess ON commands(session_id, t_start);

-- Output chunks (~16 KB / 128 logical lines). command_id NULL: unattributed (`gap`) output.
CREATE TABLE outputs(
  id INTEGER PRIMARY KEY,
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  command_id INT REFERENCES commands ON DELETE CASCADE,
  t REAL NOT NULL,                   -- first write of the chunk's first line (jump-to-hit)
  ts REAL NOT NULL,
  line0 INT,                         -- ordinal of the first line in its owner's transcript
  kind TEXT NOT NULL CHECK(kind IN('primary','alt','gap','provisional')),
  text TEXT NOT NULL);
CREATE INDEX outputs_cmd ON outputs(command_id);
CREATE INDEX outputs_sess ON outputs(session_id, t);
CREATE INDEX outputs_ts ON outputs(ts);
CREATE INDEX outputs_provisional ON outputs(session_id) WHERE kind = 'provisional';

CREATE VIRTUAL TABLE cmd_fts USING fts5(typed, expanded, cwd,
  content='commands', content_rowid='id', tokenize='trigram', detail='none');
CREATE VIRTUAL TABLE out_fts USING fts5(text,
  content='outputs', content_rowid='id', tokenize='trigram', detail='none');

CREATE TRIGGER commands_ai AFTER INSERT ON commands BEGIN
  INSERT INTO cmd_fts(rowid, typed, expanded, cwd) VALUES(new.id, new.typed, new.expanded, new.cwd);
END;
CREATE TRIGGER commands_ad AFTER DELETE ON commands BEGIN
  INSERT INTO cmd_fts(cmd_fts, rowid, typed, expanded, cwd)
    VALUES('delete', old.id, old.typed, old.expanded, old.cwd);
END;
CREATE TRIGGER commands_au AFTER UPDATE OF typed, expanded, cwd ON commands BEGIN
  INSERT INTO cmd_fts(cmd_fts, rowid, typed, expanded, cwd)
    VALUES('delete', old.id, old.typed, old.expanded, old.cwd);
  INSERT INTO cmd_fts(rowid, typed, expanded, cwd) VALUES(new.id, new.typed, new.expanded, new.cwd);
END;

CREATE TRIGGER outputs_ai AFTER INSERT ON outputs BEGIN
  INSERT INTO out_fts(rowid, text) VALUES(new.id, new.text);
END;
CREATE TRIGGER outputs_ad AFTER DELETE ON outputs BEGIN
  INSERT INTO out_fts(out_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER outputs_au AFTER UPDATE OF text ON outputs BEGIN
  INSERT INTO out_fts(out_fts, rowid, text) VALUES('delete', old.id, old.text);
  INSERT INTO out_fts(rowid, text) VALUES(new.id, new.text);
END;

-- Non-chapter markers kept for the timeline (§4.2): pause, resume, nested, multiplexer, ctx,
-- gap, rotate, error, clock, skip. data_json is the `m` payload (never contains the nonce).
CREATE TABLE markers(
  id INTEGER PRIMARY KEY,
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  t REAL NOT NULL,
  ts REAL NOT NULL,
  kind TEXT NOT NULL,
  data_json TEXT NOT NULL);
CREATE INDEX markers_sess ON markers(session_id, t);

-- Terminal activity per session and wall-clock minute (counts only, no content): what the
-- timeline draws where no command boundaries exist (inside ssh, an editor, a nested shell).
CREATE TABLE activity(
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  minute INT NOT NULL,               -- floor(ts / 60)
  inputs INT NOT NULL DEFAULT 0,     -- recorded `i` events (keystrokes, pastes)
  out_bytes INT NOT NULL DEFAULT 0,  -- bytes of `o` events
  PRIMARY KEY(session_id, minute)) WITHOUT ROWID;

-- Playback segments for the player's segmented format (§7.5).
CREATE TABLE segments(
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  seq INT NOT NULL,                  -- 0-based, in time order across parts
  part INT NOT NULL,
  t_start REAL NOT NULL,
  t_end REAL,
  byte_offset INT NOT NULL,          -- uncompressed line offset in `part`
  vt_dump BLOB,                      -- terminal state at t_start (UTF-8)
  PRIMARY KEY(session_id, seq));

-- Frame table of compressed parts (§6.6), for seeking to a checkpoint or segment offset.
CREATE TABLE zst_frames(
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  part INT NOT NULL,
  seq INT NOT NULL,
  c_offset INT NOT NULL,
  c_len INT NOT NULL,
  u_offset INT NOT NULL,
  u_len INT NOT NULL,
  PRIMARY KEY(session_id, part, seq));

-- One row per legacy cast seen by the importer (§11). Aggregates only, never content.
CREATE TABLE legacy_import(
  path TEXT PRIMARY KEY,
  size INT,
  mtime REAL,
  ino INT,
  sha256 TEXT,                       -- hex sha256 of the first 1 MiB
  commands_log TEXT,
  status TEXT NOT NULL CHECK(status IN('imported','hidden','skipped','failed')),
  session_id INT REFERENCES sessions ON DELETE SET NULL,
  banners INT,
  log_lines INT,
  aligned INT,
  mean_conf REAL,
  error TEXT,
  updated_at REAL);

-- Free-form indexer facts for `cinelog status` (keys: model::info_keys).
CREATE TABLE index_info(key TEXT PRIMARY KEY, value TEXT);
