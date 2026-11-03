//! Durable per-video policy for deferring automatic Wiki ingestion.

use sqlx::Row;

use crate::error::TaskResult;
use crate::store::codec::{db_err, now_ms};
use crate::store::TaskStore;

impl TaskStore {
    pub(crate) async fn defer_wiki_ingest(&self, video_id: &str) -> TaskResult<()> {
        sqlx::query(
            "INSERT INTO deferred_wiki_ingest(video_id, source_rel_path, updated_at_ms)
             VALUES (?1, NULL, ?2)
             ON CONFLICT(video_id) DO UPDATE SET updated_at_ms = excluded.updated_at_ms",
        )
        .bind(video_id)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(db_err("defer Wiki source ingestion"))?;
        Ok(())
    }

    pub(crate) async fn record_deferred_wiki_source_path(
        &self,
        video_id: &str,
        source_rel_path: &str,
    ) -> TaskResult<()> {
        sqlx::query(
            "UPDATE deferred_wiki_ingest SET source_rel_path = ?1, updated_at_ms = ?2
             WHERE video_id = ?3",
        )
        .bind(source_rel_path)
        .bind(now_ms())
        .bind(video_id)
        .execute(&self.pool)
        .await
        .map_err(db_err("record deferred Wiki source path"))?;
        Ok(())
    }

    pub(crate) async fn resume_wiki_ingest_for_video(&self, video_id: &str) -> TaskResult<()> {
        sqlx::query("DELETE FROM deferred_wiki_ingest WHERE video_id = ?1")
            .bind(video_id)
            .execute(&self.pool)
            .await
            .map_err(db_err("resume Wiki source ingestion"))?;
        Ok(())
    }

    pub(crate) async fn resume_wiki_ingest_for_path(
        &self,
        source_rel_path: &str,
    ) -> TaskResult<()> {
        sqlx::query("DELETE FROM deferred_wiki_ingest WHERE source_rel_path = ?1")
            .bind(source_rel_path)
            .execute(&self.pool)
            .await
            .map_err(db_err("resume Wiki source ingestion by path"))?;
        Ok(())
    }

    pub(crate) async fn deferred_wiki_ingest_video_ids(&self) -> TaskResult<Vec<String>> {
        let rows = sqlx::query("SELECT video_id FROM deferred_wiki_ingest ORDER BY video_id")
            .fetch_all(&self.pool)
            .await
            .map_err(db_err("list deferred Wiki source ingestion"))?;
        Ok(rows.iter().map(|row| row.get("video_id")).collect())
    }

    pub(crate) async fn deferred_wiki_ingest_paths(&self) -> TaskResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT source_rel_path FROM deferred_wiki_ingest
             WHERE source_rel_path IS NOT NULL ORDER BY source_rel_path",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("list deferred Wiki source paths"))?;
        Ok(rows.iter().map(|row| row.get("source_rel_path")).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deferred_wiki_ingest_survives_store_reopen_and_resumes_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).await.unwrap();
        store.defer_wiki_ingest("video-one").await.unwrap();
        store
            .record_deferred_wiki_source_path("video-one", "sources/topic/video-one.md")
            .await
            .unwrap();
        store.defer_wiki_ingest("video-two").await.unwrap();
        store.close().await;
        drop(store);

        let reopened = TaskStore::open(&db_path).await.unwrap();
        assert_eq!(
            reopened.deferred_wiki_ingest_video_ids().await.unwrap(),
            ["video-one", "video-two"]
        );
        reopened
            .resume_wiki_ingest_for_path("sources/topic/video-one.md")
            .await
            .unwrap();
        assert_eq!(
            reopened.deferred_wiki_ingest_video_ids().await.unwrap(),
            ["video-two"]
        );
        reopened.close().await;
    }
}
