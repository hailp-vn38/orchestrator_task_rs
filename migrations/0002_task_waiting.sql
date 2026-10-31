ALTER TABLE tasks ADD COLUMN wait_reason TEXT;
ALTER TABLE tasks ADD COLUMN next_attempt_at INTEGER;
