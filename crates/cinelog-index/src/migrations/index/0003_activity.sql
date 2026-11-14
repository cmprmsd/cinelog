-- v3: terminal activity per session and wall-clock minute (counts only, no content).
CREATE TABLE activity(
  session_id INT NOT NULL REFERENCES sessions ON DELETE CASCADE,
  minute INT NOT NULL,               -- floor(ts / 60)
  inputs INT NOT NULL DEFAULT 0,     -- recorded `i` events (keystrokes, pastes)
  out_bytes INT NOT NULL DEFAULT 0,  -- bytes of `o` events
  PRIMARY KEY(session_id, minute)) WITHOUT ROWID;
