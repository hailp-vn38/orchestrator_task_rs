-- Sổ theo dõi thu thập: trạng thái từng video của playlist, sống qua restart và qua các session.
CREATE TABLE collection_items (
    collection_key TEXT NOT NULL,
    item_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'saved', 'no_transcript', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    source_path TEXT,
    last_error TEXT,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (collection_key, item_id)
);
CREATE INDEX idx_collection_items_item ON collection_items(item_id);

-- Số chặng Auto-Flow đã nhận một nguồn mà chưa ingest xong; đủ ngưỡng thì nguồn bị cách ly.
CREATE TABLE source_ingest_attempts (
    source_rel_path TEXT PRIMARY KEY,
    attempts INTEGER NOT NULL,
    last_task_id TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
