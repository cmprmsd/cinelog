-- v2: the pane's index within its window, as shown by tmux (`ctx` markers). Sessions indexed
-- before get it from their last stored `ctx` marker.
ALTER TABLE sessions ADD COLUMN tmux_pane_index TEXT;
UPDATE sessions SET tmux_pane_index = (
  SELECT nullif(json_extract(m.data_json, '$.pane_index'), '')
  FROM markers m
  WHERE m.session_id = sessions.id AND m.kind = 'ctx' AND json_valid(m.data_json)
  ORDER BY m.t DESC, m.id DESC LIMIT 1)
WHERE tmux_pane IS NOT NULL;
