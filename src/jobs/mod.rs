//! Apalis worker setup for FeedRelay.
//!
//! Provides:
//! - [`setup_storage`] — runs Apalis' own SQLite migrations once at boot
//! - [`build_storage`] — creates a typed `SqliteStorage<Run>` for enqueueing (Task 6)
//! - [`enqueue_run`] — pushes a `Run` job into the queue (Task 6 calls this)
//! - [`run_worker`] — builds and drives the Apalis polling worker

pub mod run;

use apalis::layers::WorkerBuilderExt;
use apalis::layers::retry::RetryPolicy;
use apalis::prelude::WorkerBuilder;
use apalis_sqlite::SqliteStorage;
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::jobs::run::{Run, make_run_handler};
use crate::pipeline::PipelineContext;

// ---------------------------------------------------------------------------
// Public error type — hides the Apalis trait types from callers
// ---------------------------------------------------------------------------

/// Error returned when enqueueing a `Run` job fails.
///
/// Wraps the underlying Apalis/SQLite error without leaking `apalis::prelude`
/// types into Task 6's call sites.
#[derive(Debug, thiserror::Error)]
#[error("failed to enqueue run: {0}")]
pub struct EnqueueError(#[source] Box<dyn std::error::Error + Send + Sync + 'static>);

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Run Apalis' own SQLite schema migrations.
///
/// `SqliteStorage::setup` is idempotent — safe to call on every boot.
/// We run this before our own `sqlx::migrate!` to keep the ordering
/// deterministic across fresh installs.
pub async fn setup_storage(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut migrator = SqliteStorage::<(), (), ()>::migrations();
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}

/// Push a `Run` job into the Apalis queue so the worker picks it up.
///
/// Task 6 calls this after inserting the `runs` row.  Accepts any storage
/// returned by [`build_storage`]; callers never need to import `apalis::prelude`.
#[allow(dead_code)] // used by Task 6
pub async fn enqueue_run<S>(storage: &mut S, run: Run) -> Result<(), EnqueueError>
where
    S: apalis::prelude::TaskSink<Run, Error = sqlx::Error>,
{
    storage
        .push(run)
        .await
        .map_err(|e| EnqueueError(Box::new(e)))
}

/// Get a storage instance for enqueueing jobs.
///
/// Task 6 calls `build_storage` once at startup and passes the result to
/// [`enqueue_run`].  The return type is opaque — Task 6 never needs to import
/// `apalis::prelude` or `apalis_sqlite`.
#[allow(dead_code)] // used by Task 6
pub fn build_storage(
    pool: &SqlitePool,
) -> impl apalis::prelude::TaskSink<Run, Error = sqlx::Error> + Clone {
    let storage: SqliteStorage<Run, _, _> = SqliteStorage::new(pool);
    storage
}

/// Build and run the Apalis worker, polling for `Run` jobs.
///
/// Concurrency = 1: a single `WorkerBuilder` with no `parallelize` layer means
/// Apalis processes one job at a time — one Run through the pipeline at a time.
///
/// Retry policy = 0: a failed job is **not** retried. A Run only succeeds on its
/// first attempt; otherwise it is marked failed (the pipeline already records
/// the failure in our own `runs` table before returning the error).
///
/// The pipeline context is wrapped in `Arc` and captured by the handler closure,
/// so there is no per-task heap allocation for the shared state.
///
/// Returns when the worker terminates (normally or on error).
pub async fn run_worker(
    pool: &SqlitePool,
    ctx: PipelineContext,
) -> Result<(), apalis::prelude::WorkerError> {
    // Let type inference determine the full SqliteStorage<Run, C, F> type.
    let backend: SqliteStorage<Run, _, _> = SqliteStorage::new(pool);

    // Wrap context in Arc so the closure can clone it cheaply per-job.
    let ctx = Arc::new(ctx);
    let handler = make_run_handler(ctx);

    let worker = WorkerBuilder::new("feedrelay-runner")
        .backend(backend)
        .retry(RetryPolicy::retries(0))
        .build(handler);

    worker.run().await
}
