//! Apalis job definition and handler for the FeedRelay Run pipeline.
//!
//! This module is intentionally thin — all pipeline logic lives in
//! `crate::pipeline::execute_run`.  The handler just extracts the job payload
//! and the shared context, then delegates.

use apalis::prelude::BoxDynError;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::pipeline::{PipelineContext, execute_run};

/// The payload Apalis persists in the SQLite `Jobs` table between enqueue and pickup.
///
/// Kept small and JSON-serialisable.  `run_id` is the ULID that also appears in
/// our own `runs` table.  `query_hint` overrides the default CLIP query when set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    /// ULID that identifies this run in our `runs` table.
    pub run_id: String,
    /// Optional CLIP query override (e.g. from the `/trigger/post` body).
    pub query_hint: Option<String>,
}

/// Build a handler closure that captures the `PipelineContext` in an `Arc`.
///
/// We use a closure that captures the context rather than the `Data<T>` extractor
/// pattern.  This avoids the `Data<T>` middleware layer while keeping the handler
/// testable: `pipeline::execute_run` is called directly from tests without Apalis.
pub fn make_run_handler(
    ctx: Arc<PipelineContext>,
) -> impl Fn(Run) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BoxDynError>> + Send>>
+ Clone
+ Send
+ 'static {
    move |job: Run| {
        let ctx = ctx.clone();
        Box::pin(async move {
            let run_id = job.run_id.clone();
            let query_hint = job.query_hint.clone();

            tracing::info!(
                run_id = %run_id,
                query_hint = ?query_hint,
                "apalis: picked up run job"
            );

            match execute_run(&ctx, &run_id, query_hint.as_deref()).await {
                Ok(success) => {
                    tracing::info!(
                        run_id = %run_id,
                        buffer_post_id = %success.buffer_post_id,
                        "apalis: run completed successfully"
                    );
                    Ok(())
                }
                Err(e) => {
                    // Pipeline already wrote 'failed' status to the DB.
                    // Return error so Apalis logs it; retry policy is on the worker.
                    tracing::error!(
                        run_id = %run_id,
                        error = %e,
                        "apalis: run failed"
                    );
                    Err(e.to_string().into())
                }
            }
        })
    }
}
