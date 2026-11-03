-- A durable operator choice to keep a collected source out of automatic Wiki ingestion.
CREATE TABLE deferred_wiki_ingest (
    video_id TEXT PRIMARY KEY,
    source_rel_path TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_deferred_wiki_ingest_path
    ON deferred_wiki_ingest(source_rel_path);
