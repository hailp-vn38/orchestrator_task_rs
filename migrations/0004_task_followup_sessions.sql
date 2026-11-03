ALTER TABLE tasks ADD COLUMN active_session_id TEXT;
ALTER TABLE tasks ADD COLUMN followup_session_id TEXT;
UPDATE tasks SET active_session_id = session_id WHERE active_session_id IS NULL;
