//! Sổ theo dõi thu thập (`collection_items`) và số lần ingest lỗi theo nguồn (`source_ingest_attempts`).

use sqlx::Row;

use crate::error::TaskResult;
use crate::store::codec::{db_err, now_ms};
use crate::store::TaskStore;

pub(crate) const ITEM_SAVED: &str = "saved";
pub(crate) const ITEM_NO_TRANSCRIPT: &str = "no_transcript";
pub(crate) const ITEM_FAILED: &str = "failed";

/// Một video trong sổ theo dõi của playlist.
#[derive(Debug, Clone)]
pub(crate) struct CollectionItemRow {
    pub item_id: String,
    pub position: i64,
    pub title: String,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
}

impl TaskStore {
    /// Ghi nhận danh sách video của playlist. Video đã có giữ nguyên trạng thái và số lần thử.
    pub(crate) async fn upsert_collection_items(
        &self,
        collection_key: &str,
        items: &[(String, String)],
    ) -> TaskResult<()> {
        let now = now_ms();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db_err("begin collection upsert"))?;
        for (index, (item_id, title)) in items.iter().enumerate() {
            sqlx::query(
                "INSERT INTO collection_items
                    (collection_key, item_id, position, title, status, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5)
                 ON CONFLICT (collection_key, item_id) DO UPDATE SET
                    position = excluded.position, title = excluded.title",
            )
            .bind(collection_key)
            .bind(item_id)
            .bind(index as i64 + 1)
            .bind(title)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err("upsert collection item"))?;
        }
        tx.commit()
            .await
            .map_err(db_err("commit collection upsert"))?;
        Ok(())
    }

    /// Cập nhật kết quả của một video ở mọi playlist chứa nó. Chỉ lỗi `failed` mới tăng số lần thử.
    pub(crate) async fn record_collection_outcome(
        &self,
        item_id: &str,
        status: &str,
        source_path: Option<&str>,
        error: Option<&str>,
    ) -> TaskResult<()> {
        sqlx::query(
            "UPDATE collection_items SET
                status = ?1,
                source_path = COALESCE(?2, source_path),
                last_error = ?3,
                attempts = attempts + CASE WHEN ?1 = 'failed' THEN 1 ELSE 0 END,
                updated_at_ms = ?4
             WHERE item_id = ?5",
        )
        .bind(status)
        .bind(source_path)
        .bind(error)
        .bind(now_ms())
        .bind(item_id)
        .execute(&self.pool)
        .await
        .map_err(db_err("record collection outcome"))?;
        Ok(())
    }

    pub(crate) async fn list_collection_items(
        &self,
        collection_key: &str,
    ) -> TaskResult<Vec<CollectionItemRow>> {
        let rows = sqlx::query(
            "SELECT item_id, position, title, status, attempts, last_error
             FROM collection_items WHERE collection_key = ?1 ORDER BY position ASC",
        )
        .bind(collection_key)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("list collection items"))?;
        Ok(rows
            .iter()
            .map(|r| CollectionItemRow {
                item_id: r.get("item_id"),
                position: r.get("position"),
                title: r.get("title"),
                status: r.get("status"),
                attempts: r.get("attempts"),
                last_error: r.get("last_error"),
            })
            .collect())
    }

    /// Tiêu đề và playlist đã biết của một video, dùng được cả sau khi process khởi động lại.
    pub(crate) async fn collection_item_identity(
        &self,
        item_id: &str,
    ) -> TaskResult<Option<(String, String, i64)>> {
        let row = sqlx::query(
            "SELECT title, collection_key, position FROM collection_items
             WHERE item_id = ?1 ORDER BY updated_at_ms DESC LIMIT 1",
        )
        .bind(item_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("find collection item"))?;
        Ok(row.map(|r| (r.get("title"), r.get("collection_key"), r.get("position"))))
    }

    /// Nguồn đã thất bại đủ `max_attempts` chặng: bị loại khỏi hàng đợi cho tới khi được giải phóng.
    pub(crate) async fn quarantined_sources(&self, max_attempts: u32) -> TaskResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT source_rel_path FROM source_ingest_attempts
             WHERE attempts >= ?1 ORDER BY source_rel_path ASC",
        )
        .bind(i64::from(max_attempts))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("list quarantined sources"))?;
        Ok(rows.iter().map(|r| r.get("source_rel_path")).collect())
    }

    /// Tăng số lần thất bại của các nguồn một chặng đã nhận nhưng chưa ingest xong.
    pub(crate) async fn bump_source_ingest_attempts(
        &self,
        sources: &[String],
        task_id: &str,
    ) -> TaskResult<()> {
        let now = now_ms();
        for source in sources {
            sqlx::query(
                "INSERT INTO source_ingest_attempts
                    (source_rel_path, attempts, last_task_id, updated_at_ms)
                 VALUES (?1, 1, ?2, ?3)
                 ON CONFLICT (source_rel_path) DO UPDATE SET
                    attempts = attempts + 1,
                    last_task_id = excluded.last_task_id,
                    updated_at_ms = excluded.updated_at_ms",
            )
            .bind(source)
            .bind(task_id)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err("bump source ingest attempts"))?;
        }
        Ok(())
    }

    /// Xoá bộ đếm của nguồn đã ingest xong hoặc được người vận hành giải phóng khỏi cách ly.
    pub(crate) async fn clear_source_ingest_attempts(&self, sources: &[String]) -> TaskResult<u64> {
        let mut cleared = 0;
        for source in sources {
            cleared += sqlx::query("DELETE FROM source_ingest_attempts WHERE source_rel_path = ?1")
                .bind(source)
                .execute(&self.pool)
                .await
                .map_err(db_err("clear source ingest attempts"))?
                .rows_affected();
        }
        Ok(cleared)
    }
}
