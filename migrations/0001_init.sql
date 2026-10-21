-- 0001_init.sql: Schema cho tasks.db (Task, Followup, Schedule & Reminder)

CREATE TABLE IF NOT EXISTS tasks (
    id                  TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL DEFAULT 'personal',
    workspace_id        TEXT NOT NULL DEFAULT 'homelab',
    title               TEXT NOT NULL,
    prompt              TEXT NOT NULL,
    flow_id             TEXT NOT NULL,
    agent_id            TEXT NOT NULL,
    source              TEXT NOT NULL,
    schedule_id         TEXT,
    session_id          TEXT NOT NULL,
    latest_turn_id      TEXT,
    status              TEXT NOT NULL,
    active_subagent     TEXT,
    active_tool         TEXT,
    progress_summary    TEXT,
    output_preview      TEXT,
    related_wiki_links  TEXT NOT NULL DEFAULT '[]',
    error_code          TEXT,
    error_message       TEXT,
    created_at          INTEGER NOT NULL,
    started_at          INTEGER,
    updated_at          INTEGER NOT NULL,
    finished_at         INTEGER
);

CREATE INDEX IF NOT EXISTS idx_tasks_scope_status_created
    ON tasks (owner_id, workspace_id, status, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_scope_created
    ON tasks (owner_id, workspace_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_tasks_flow_session
    ON tasks (flow_id, session_id);

CREATE TABLE IF NOT EXISTS task_followups (
    id                  TEXT PRIMARY KEY,
    task_id             TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    prompt              TEXT NOT NULL,
    source              TEXT NOT NULL,
    status              TEXT NOT NULL,
    submitted_turn_id   TEXT,
    created_at          INTEGER NOT NULL,
    submitted_at        INTEGER
);

CREATE INDEX IF NOT EXISTS idx_followups_task_pending
    ON task_followups (task_id, status, created_at ASC);

CREATE TABLE IF NOT EXISTS task_schedules (
    id                  TEXT PRIMARY KEY,
    owner_id            TEXT NOT NULL DEFAULT 'personal',
    workspace_id        TEXT NOT NULL DEFAULT 'homelab',
    title               TEXT NOT NULL,
    notes               TEXT,
    action_kind         TEXT NOT NULL,
    flow_id             TEXT,
    agent_id            TEXT,
    session_mode        TEXT NOT NULL DEFAULT 'new_session',
    reused_session_id   TEXT,
    priority            TEXT NOT NULL DEFAULT 'medium',
    related_wiki_links  TEXT NOT NULL DEFAULT '[]',
    schedule_kind       TEXT NOT NULL,
    due_at              INTEGER,
    cron_expr           TEXT,
    timezone            TEXT NOT NULL DEFAULT 'Asia/Ho_Chi_Minh',
    status              TEXT NOT NULL DEFAULT 'pending',
    resolution_note     TEXT,
    next_run_at         INTEGER,
    last_run_at         INTEGER,
    last_task_id        TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_schedules_due
    ON task_schedules (status, next_run_at ASC);

CREATE INDEX IF NOT EXISTS idx_schedules_scope_status
    ON task_schedules (owner_id, workspace_id, status, created_at DESC);
