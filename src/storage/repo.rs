//! Repository layer: CRUD operations on app-owned SQLite tables.
//!
//! # Why no `sqlx::query!` macros?
//!
//! The compile-time `sqlx::query!` macros require either a live `DATABASE_URL`
//! or a pre-generated `.sqlx/` query cache (`cargo sqlx prepare`).  Neither is
//! practical for this project's CI / local-dev flow yet.  We use the runtime
//! `sqlx::query()` / `sqlx::Row` approach instead; the schema is still validated
//! by the integration tests which run against an in-memory SQLite with migrations
//! applied.

use std::collections::HashSet;

use crate::caption::Caption;
use chrono::Utc;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

// ---------------------------------------------------------------------------
// Caption cache
// ---------------------------------------------------------------------------

/// Look up a cached caption by `(asset_id, rendered_prompt)`.
///
/// Returns `Ok(Some(caption))` on a cache hit, `Ok(None)` on a miss.
///
/// The `rendered_prompt` is the full composed cache key (system + user prompt).
/// See [`crate::caption::compose_cache_key`] for the stable format.
pub async fn caption_lookup(
    pool: &SqlitePool,
    asset_id: &str,
    rendered_prompt: &str,
) -> Result<Option<Caption>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT caption, hashtags, alt_text \
         FROM captions \
         WHERE immich_asset_id = ? AND prompt = ?",
    )
    .bind(asset_id)
    .bind(rendered_prompt)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => {
            let caption_text: String = r.try_get("caption")?;
            let hashtags_json: String = r.try_get("hashtags")?;
            let alt_text: String = r.try_get("alt_text")?;

            let hashtags: Vec<String> = serde_json::from_str(&hashtags_json).map_err(|e| {
                // Wrap the serde error as a sqlx::Error::Decode so callers see a uniform type.
                sqlx::Error::Decode(format!("failed to deserialise hashtags JSON: {e}").into())
            })?;

            Ok(Some(Caption {
                text: caption_text,
                hashtags,
                alt_text,
            }))
        }
    }
}

/// Insert (or replace) a caption cache row.
///
/// Uses `INSERT OR REPLACE` because the composite PK `(immich_asset_id, prompt)`
/// ensures idempotence: a race between two workers on the same `(asset, prompt)`
/// pair is harmless — both produce equivalent captions for the same prompt.
///
/// `hashtags` is serialised as a JSON array string in the `hashtags TEXT` column.
/// `generated_at` is set to the current unix epoch (seconds).
pub async fn caption_insert(
    pool: &SqlitePool,
    asset_id: &str,
    rendered_prompt: &str,
    caption: &Caption,
) -> Result<(), sqlx::Error> {
    let hashtags_json = serde_json::to_string(&caption.hashtags)
        .expect("Vec<String> JSON serialisation is infallible");
    let generated_at = Utc::now().timestamp();

    sqlx::query(
        "INSERT OR REPLACE INTO captions \
             (immich_asset_id, prompt, caption, hashtags, alt_text, generated_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(asset_id)
    .bind(rendered_prompt)
    .bind(&caption.text)
    .bind(&hashtags_json)
    .bind(&caption.alt_text)
    .bind(generated_at)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// A row from the `runs` table.
///
/// Used by Task 6's `/trigger/post` status endpoint.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct RunRow {
    pub run_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub query_used: Option<String>,
    pub candidates_returned: Option<i64>,
    pub candidates_after_filter: Option<i64>,
    pub selected_asset_id: Option<String>,
    pub caption: Option<String>,
    pub buffer_post_id: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Fields required when marking a run as succeeded.
#[derive(Debug)]
pub struct RunSuccessFields<'a> {
    pub run_id: &'a str,
    pub finished_at: i64,
    pub query_used: &'a str,
    pub candidates_returned: i64,
    pub candidates_after_filter: i64,
    pub selected_asset_id: &'a str,
    pub caption: &'a str,
    pub buffer_post_id: &'a str,
    pub duration_ms: i64,
}

/// Insert a new run row with `status='queued'`.
///
/// Called at enqueue time (Task 6 will call this before pushing to Apalis).
/// `started_at` is the epoch-seconds when the request was received.
pub async fn run_insert(
    pool: &SqlitePool,
    run_id: &str,
    started_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO runs (run_id, started_at, status) VALUES (?, ?, 'queued')")
        .bind(run_id)
        .bind(started_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Retrieve a run row by `run_id`.  Returns `None` if not found.
pub async fn run_get_status(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<RunRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT run_id, started_at, finished_at, status, query_used, \
         candidates_returned, candidates_after_filter, selected_asset_id, \
         caption, buffer_post_id, error, duration_ms \
         FROM runs WHERE run_id = ?",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(RunRow {
            run_id: r.try_get("run_id")?,
            started_at: r.try_get("started_at")?,
            finished_at: r.try_get("finished_at")?,
            status: r.try_get("status")?,
            query_used: r.try_get("query_used")?,
            candidates_returned: r.try_get("candidates_returned")?,
            candidates_after_filter: r.try_get("candidates_after_filter")?,
            selected_asset_id: r.try_get("selected_asset_id")?,
            caption: r.try_get("caption")?,
            buffer_post_id: r.try_get("buffer_post_id")?,
            error: r.try_get("error")?,
            duration_ms: r.try_get("duration_ms")?,
        })),
    }
}

/// Return the `(run_id, status)` of any run currently `'queued'` or `'running'`,
/// or `None` if no run is in-flight.
///
/// Task 6 uses this to enforce the one-in-flight gate (returns 409 if Some).
pub async fn run_in_flight(pool: &SqlitePool) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT run_id, status FROM runs \
         WHERE status IN ('queued', 'running') \
         ORDER BY started_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some(r) => {
            let run_id: String = r.try_get("run_id")?;
            let status: String = r.try_get("status")?;
            Ok(Some((run_id, status)))
        }
    }
}

/// Transition a run from `'queued'` to `'running'`.
///
/// `started_at` is coalesced so it captures the worker pickup time only if it
/// was not already set at enqueue time.
pub async fn run_mark_running(pool: &SqlitePool, run_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE runs SET status = 'running', \
         started_at = COALESCE(started_at, strftime('%s','now')) \
         WHERE run_id = ?",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Transition a run to `'succeeded'` and write all audit fields, inside an
/// open transaction.
///
/// Use this together with [`posts_insert_in_tx`] so that both the `posts`
/// INSERT and the `runs` UPDATE are committed atomically.
pub async fn run_mark_succeeded_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    fields: &RunSuccessFields<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE runs SET \
         status = 'succeeded', \
         finished_at = ?, \
         query_used = ?, \
         candidates_returned = ?, \
         candidates_after_filter = ?, \
         selected_asset_id = ?, \
         caption = ?, \
         buffer_post_id = ?, \
         duration_ms = ? \
         WHERE run_id = ?",
    )
    .bind(fields.finished_at)
    .bind(fields.query_used)
    .bind(fields.candidates_returned)
    .bind(fields.candidates_after_filter)
    .bind(fields.selected_asset_id)
    .bind(fields.caption)
    .bind(fields.buffer_post_id)
    .bind(fields.duration_ms)
    .bind(fields.run_id)
    .execute(tx.as_mut())
    .await?;
    Ok(())
}

/// Transition a run to `'failed'` and write the error message.
pub async fn run_mark_failed(
    pool: &SqlitePool,
    run_id: &str,
    error: &str,
    duration_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE runs SET \
         status = 'failed', \
         finished_at = strftime('%s', 'now'), \
         error = ?, \
         duration_ms = ? \
         WHERE run_id = ?",
    )
    .bind(error)
    .bind(duration_ms)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Posts
// ---------------------------------------------------------------------------

/// Load all previously-posted `immich_asset_id` values into a `HashSet`.
///
/// This is the dedup set used by [`crate::filter::classify_candidate`].
pub async fn posts_dedup_set(pool: &SqlitePool) -> Result<HashSet<String>, sqlx::Error> {
    let rows = sqlx::query("SELECT immich_asset_id FROM posts")
        .fetch_all(pool)
        .await?;

    let set = rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("immich_asset_id"))
        .collect::<Result<HashSet<_>, _>>()?;

    Ok(set)
}

/// Insert a post row inside an open transaction.
///
/// `posted_at` is unix epoch seconds.
pub async fn posts_insert_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    immich_asset_id: &str,
    buffer_post_id: &str,
    caption: &str,
    posted_at: i64,
    run_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO posts \
         (immich_asset_id, buffer_post_id, caption, posted_at, run_id) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(immich_asset_id)
    .bind(buffer_post_id)
    .bind(caption)
    .bind(posted_at)
    .bind(run_id)
    .execute(tx.as_mut())
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pending media
// ---------------------------------------------------------------------------

/// A row from the `pending_media` table.
///
/// Used by Task 6's `/pic/{uuid}.jpg` route.
#[derive(Debug, Clone)]
pub struct PendingMediaRow {
    #[allow(dead_code)]
    pub uuid: String,
    pub immich_asset_id: String,
    pub expires_at: i64,
}

/// Delete all `pending_media` rows whose `expires_at` is in the past.
///
/// `now_epoch` is the current unix timestamp (seconds).
/// Returns the number of rows deleted.
pub async fn pending_media_cleanup(pool: &SqlitePool, now_epoch: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM pending_media WHERE expires_at < ?")
        .bind(now_epoch)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Insert a `pending_media` row.
///
/// `expires_at` is the unix epoch second after which this row is eligible for
/// cleanup.  Compute it as `now + ttl_seconds`.
pub async fn pending_media_insert(
    pool: &SqlitePool,
    uuid: &str,
    asset_id: &str,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO pending_media (uuid, immich_asset_id, expires_at) VALUES (?, ?, ?)")
        .bind(uuid)
        .bind(asset_id)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Look up a `pending_media` row by `uuid`.  Used by Task 6's `/pic/{uuid}.jpg`.
pub async fn pending_media_lookup(
    pool: &SqlitePool,
    uuid: &str,
) -> Result<Option<PendingMediaRow>, sqlx::Error> {
    let row =
        sqlx::query("SELECT uuid, immich_asset_id, expires_at FROM pending_media WHERE uuid = ?")
            .bind(uuid)
            .fetch_optional(pool)
            .await?;

    match row {
        None => Ok(None),
        Some(r) => Ok(Some(PendingMediaRow {
            uuid: r.try_get("uuid")?,
            immich_asset_id: r.try_get("immich_asset_id")?,
            expires_at: r.try_get("expires_at")?,
        })),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::test_pool;

    #[tokio::test]
    async fn caption_lookup_returns_none_on_empty_db() {
        let pool = test_pool().await;
        let result = caption_lookup(&pool, "asset-1", "some-prompt")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn caption_insert_and_lookup_round_trips() {
        let pool = test_pool().await;

        let caption = Caption {
            text: "Golden hour in the hills.".into(),
            hashtags: vec!["sintra".into(), "portugal".into(), "sunset".into()],
            alt_text: "Rolling hills at sunset.".into(),
        };

        caption_insert(&pool, "asset-2", "prompt-key-42", &caption)
            .await
            .unwrap();

        let fetched = caption_lookup(&pool, "asset-2", "prompt-key-42")
            .await
            .unwrap()
            .expect("expected a row");

        assert_eq!(fetched, caption);
    }

    #[tokio::test]
    async fn caption_insert_or_replace_is_idempotent() {
        let pool = test_pool().await;

        let first = Caption {
            text: "First caption.".into(),
            hashtags: vec!["a".into()],
            alt_text: "First alt.".into(),
        };
        let second = Caption {
            text: "Second caption.".into(),
            hashtags: vec!["b".into(), "c".into()],
            alt_text: "Second alt.".into(),
        };

        caption_insert(&pool, "asset-3", "same-key", &first)
            .await
            .unwrap();
        caption_insert(&pool, "asset-3", "same-key", &second)
            .await
            .unwrap();

        let fetched = caption_lookup(&pool, "asset-3", "same-key")
            .await
            .unwrap()
            .expect("expected a row");

        // Second write wins (INSERT OR REPLACE)
        assert_eq!(fetched, second);
    }

    // -----------------------------------------------------------------------
    // Runs
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_insert_and_get_status_round_trips() {
        let pool = test_pool().await;

        run_insert(&pool, "run-01", 1_700_000_000).await.unwrap();
        let row = run_get_status(&pool, "run-01").await.unwrap().unwrap();

        assert_eq!(row.run_id, "run-01");
        assert_eq!(row.status, "queued");
        assert_eq!(row.started_at, 1_700_000_000);
        assert!(row.finished_at.is_none());
    }

    #[tokio::test]
    async fn run_mark_running_updates_status() {
        let pool = test_pool().await;

        run_insert(&pool, "run-02", 1_700_000_001).await.unwrap();
        run_mark_running(&pool, "run-02").await.unwrap();
        let row = run_get_status(&pool, "run-02").await.unwrap().unwrap();
        assert_eq!(row.status, "running");
    }

    #[tokio::test]
    async fn run_mark_succeeded_writes_all_fields() {
        let pool = test_pool().await;

        run_insert(&pool, "run-03", 1_700_000_002).await.unwrap();
        let fields = RunSuccessFields {
            run_id: "run-03",
            finished_at: 1_700_000_100,
            query_used: "landscape",
            candidates_returned: 30,
            candidates_after_filter: 5,
            selected_asset_id: "asset-abc",
            caption: "A beautiful view.",
            buffer_post_id: "buf_post_123",
            duration_ms: 98_000,
        };
        let mut tx = pool.begin().await.unwrap();
        run_mark_succeeded_in_tx(&mut tx, &fields).await.unwrap();
        tx.commit().await.unwrap();
        let row = run_get_status(&pool, "run-03").await.unwrap().unwrap();

        assert_eq!(row.status, "succeeded");
        assert_eq!(row.finished_at, Some(1_700_000_100));
        assert_eq!(row.query_used.as_deref(), Some("landscape"));
        assert_eq!(row.candidates_returned, Some(30));
        assert_eq!(row.candidates_after_filter, Some(5));
        assert_eq!(row.selected_asset_id.as_deref(), Some("asset-abc"));
        assert_eq!(row.buffer_post_id.as_deref(), Some("buf_post_123"));
        assert_eq!(row.duration_ms, Some(98_000));
    }

    #[tokio::test]
    async fn run_mark_failed_sets_error() {
        let pool = test_pool().await;

        run_insert(&pool, "run-04", 1_700_000_003).await.unwrap();
        run_mark_failed(&pool, "run-04", "immich timeout", 5_000)
            .await
            .unwrap();
        let row = run_get_status(&pool, "run-04").await.unwrap().unwrap();

        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("immich timeout"));
        assert_eq!(row.duration_ms, Some(5_000));
        assert!(row.finished_at.is_some());
    }

    #[tokio::test]
    async fn run_in_flight_returns_some_when_queued_or_running() {
        let pool = test_pool().await;

        // Insert queued + succeeded
        run_insert(&pool, "run-q", 1_700_000_010).await.unwrap();
        run_insert(&pool, "run-s", 1_700_000_011).await.unwrap();
        {
            let mut tx = pool.begin().await.unwrap();
            run_mark_succeeded_in_tx(
                &mut tx,
                &RunSuccessFields {
                    run_id: "run-s",
                    finished_at: 1_700_000_100,
                    query_used: "q",
                    candidates_returned: 1,
                    candidates_after_filter: 1,
                    selected_asset_id: "a",
                    caption: "c",
                    buffer_post_id: "b",
                    duration_ms: 1,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        // Should find the queued run
        let in_flight = run_in_flight(&pool).await.unwrap();
        assert!(in_flight.is_some());
        let (id, status) = in_flight.unwrap();
        assert_eq!(id, "run-q");
        assert_eq!(status, "queued");

        // Mark running — still in-flight
        run_mark_running(&pool, "run-q").await.unwrap();
        let in_flight = run_in_flight(&pool).await.unwrap();
        assert!(in_flight.is_some());

        // Mark failed — no more in-flight
        run_mark_failed(&pool, "run-q", "err", 1).await.unwrap();
        let in_flight = run_in_flight(&pool).await.unwrap();
        assert!(in_flight.is_none());
    }

    // -----------------------------------------------------------------------
    // Posts
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn posts_dedup_set_empty_on_fresh_db() {
        let pool = test_pool().await;
        let set = posts_dedup_set(&pool).await.unwrap();
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn posts_insert_in_tx_and_dedup_set() {
        let pool = test_pool().await;
        run_insert(&pool, "run-post", 1_700_000_020).await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        posts_insert_in_tx(
            &mut tx,
            "asset-xyz",
            "buf_post_456",
            "Great shot.",
            1_700_000_100,
            "run-post",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let set = posts_dedup_set(&pool).await.unwrap();
        assert!(set.contains("asset-xyz"));
        assert!(!set.contains("other-asset"));
    }

    // -----------------------------------------------------------------------
    // Pending media
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pending_media_cleanup_drops_expired_rows() {
        let pool = test_pool().await;
        let now = 1_700_000_000_i64;

        // Insert one expired and one fresh row
        pending_media_insert(&pool, "uuid-expired", "asset-1", now - 1)
            .await
            .unwrap();
        pending_media_insert(&pool, "uuid-fresh", "asset-2", now + 3600)
            .await
            .unwrap();

        let deleted = pending_media_cleanup(&pool, now).await.unwrap();
        assert_eq!(deleted, 1, "expected 1 expired row to be deleted");

        // Fresh row must survive
        let fresh = pending_media_lookup(&pool, "uuid-fresh").await.unwrap();
        assert!(fresh.is_some(), "fresh row should still exist");

        // Expired row must be gone
        let expired = pending_media_lookup(&pool, "uuid-expired").await.unwrap();
        assert!(expired.is_none(), "expired row should have been deleted");
    }

    #[tokio::test]
    async fn pending_media_lookup_returns_none_for_missing_uuid() {
        let pool = test_pool().await;
        let result = pending_media_lookup(&pool, "no-such-uuid").await.unwrap();
        assert!(result.is_none());
    }
}
