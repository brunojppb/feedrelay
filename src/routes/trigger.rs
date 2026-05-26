//! `POST /trigger/post` and `GET /trigger/status/{run_id}`.
//!
//! Both routes are protected by [`crate::auth::bearer_auth_middleware`], which
//! is mounted on the `/trigger` scope in `main.rs`.

use actix_web::{HttpResponse, web};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ulid::Ulid;

use crate::error::AppError;
use crate::jobs;
use crate::jobs::run::Run;
use crate::storage::repo::{RunRow, run_get_status, run_in_flight, run_insert};

// ---------------------------------------------------------------------------
// POST /trigger/post
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TriggerPostBody {
    pub query_hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct TriggerPostAccepted {
    run_id: String,
    status_url: String,
}

#[derive(Debug, Serialize)]
struct TriggerPostConflict {
    run_id: String,
    status_url: String,
    reason: &'static str,
}

/// `POST /trigger/post`
///
/// Accepts a new run request.  Returns 202 if accepted, 409 if a run is
/// already in-flight.
///
/// ## Flow
/// 1. Check for an in-flight run (queued or running). If found → 409.
/// 2. Generate a fresh ULID as `run_id`.
/// 3. Insert a `runs` row at status='queued'.
/// 4. Enqueue the `Run` job in Apalis.
/// 5. Return 202 with the run_id and status_url.
pub async fn post(
    pool: web::Data<SqlitePool>,
    body: web::Json<TriggerPostBody>,
) -> Result<HttpResponse, AppError> {
    // Step 1: one-in-flight gate.
    if let Some((existing_run_id, _)) = run_in_flight(pool.get_ref()).await? {
        let status_url = format!("/trigger/status/{existing_run_id}");
        return Ok(HttpResponse::Conflict().json(TriggerPostConflict {
            run_id: existing_run_id,
            status_url,
            reason: "run_already_in_flight",
        }));
    }

    // Step 2: fresh ULID.
    let run_id = Ulid::new().to_string();

    // Step 3: insert the run row first so the worker can find it.
    let started_at = Utc::now().timestamp();
    run_insert(pool.get_ref(), &run_id, started_at).await?;

    // Step 4: enqueue. On failure, mark the row failed so it doesn't block
    // future requests via the in-flight gate.
    let query_hint = body.into_inner().query_hint;
    // Build a fresh SqliteStorage from the pool — cheap, no connection overhead.
    let mut storage = jobs::build_storage(pool.get_ref());
    if let Err(e) = jobs::enqueue_run(
        &mut storage,
        Run {
            run_id: run_id.clone(),
            query_hint,
        },
    )
    .await
    {
        tracing::error!(run_id = %run_id, error = %e, "failed to enqueue run; marking failed");
        // Best-effort: mark the row so the gate doesn't stay locked.
        let _ = crate::storage::repo::run_mark_failed(pool.get_ref(), &run_id, "enqueue_failed", 0)
            .await;
        return Ok(HttpResponse::InternalServerError()
            .json(serde_json::json!({ "error": "enqueue_failed" })));
    }

    // Step 5: 202 Accepted.
    let status_url = format!("/trigger/status/{run_id}");
    Ok(HttpResponse::Accepted().json(TriggerPostAccepted { run_id, status_url }))
}

// ---------------------------------------------------------------------------
// GET /trigger/status/{run_id}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RunStatusResponse {
    run_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    buffer_post_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    immich_asset_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<RunRow> for RunStatusResponse {
    fn from(row: RunRow) -> Self {
        let (buffer_post_id, caption, immich_asset_id, error) = match row.status.as_str() {
            "succeeded" => (row.buffer_post_id, row.caption, row.selected_asset_id, None),
            "failed" => (None, None, None, row.error),
            _ => (None, None, None, None),
        };
        Self {
            run_id: row.run_id,
            status: row.status,
            buffer_post_id,
            caption,
            immich_asset_id,
            error,
        }
    }
}

/// `GET /trigger/status/{run_id}`
///
/// Returns the current status of a run.  Cache-Control is set to `no-store`
/// so clients always see fresh data.
pub async fn status(
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();

    match run_get_status(pool.get_ref(), &run_id).await? {
        None => Ok(HttpResponse::NotFound()
            .insert_header(("Cache-Control", "no-store"))
            .json(serde_json::json!({ "error": "run_not_found" }))),
        Some(row) => {
            let body = RunStatusResponse::from(row);
            Ok(HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(body))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthConfig, bearer_auth_middleware};
    use crate::storage::db::test_pool_with_jobs;
    use crate::storage::repo::{run_insert, run_mark_failed, run_mark_running};
    use actix_web::middleware::from_fn;
    use actix_web::test::{self, TestRequest};
    use actix_web::{App, web};

    // Build the App factory (not yet initialised into a Service).
    macro_rules! make_raw_app {
        ($pool:expr, $token:expr) => {{
            let auth_cfg = web::Data::new(AuthConfig {
                expected_token: $token.map(|s: &str| s.to_string()),
            });
            let pool_data = web::Data::new($pool.clone());

            App::new().app_data(auth_cfg).app_data(pool_data).service(
                web::scope("/trigger")
                    .wrap(from_fn(bearer_auth_middleware))
                    .route("/post", web::post().to(super::post))
                    .route("/status/{run_id}", web::get().to(super::status)),
            )
        }};
    }

    // Build a test app that wires the trigger routes with auth middleware.
    async fn make_app(
        token: Option<&str>,
    ) -> impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
    > {
        let pool = test_pool_with_jobs().await;
        test::init_service(make_raw_app!(pool, token)).await
    }

    // Build a test app AND return the pool for DB assertions.
    async fn make_app_with_pool(
        token: Option<&str>,
    ) -> (
        impl actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
            Error = actix_web::Error,
        >,
        sqlx::SqlitePool,
    ) {
        let pool = test_pool_with_jobs().await;
        let svc = test::init_service(make_raw_app!(pool, token)).await;
        (svc, pool)
    }

    // -----------------------------------------------------------------------
    // POST /trigger/post
    // -----------------------------------------------------------------------

    #[actix_web::test]
    async fn trigger_post_happy_path_returns_202_with_run_id() {
        let (app, pool) = make_app_with_pool(Some("tok")).await;

        let req = TestRequest::post()
            .uri("/trigger/post")
            .insert_header(("Authorization", "Bearer tok"))
            .insert_header(("Content-Type", "application/json"))
            .set_payload("{}")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202, "expected 202 Accepted");

        let body: serde_json::Value = test::read_body_json(resp).await;
        let run_id = body["run_id"].as_str().expect("run_id must be a string");
        assert!(!run_id.is_empty());
        assert_eq!(
            body["status_url"].as_str().unwrap(),
            format!("/trigger/status/{run_id}")
        );

        // Verify the DB row was inserted with status='queued'
        let row = run_get_status(&pool, run_id).await.unwrap();
        assert!(row.is_some(), "expected a runs row for {run_id}");
        assert_eq!(row.unwrap().status, "queued");
    }

    #[actix_web::test]
    async fn trigger_post_with_query_hint_propagates() {
        // We can't easily introspect Apalis's internal queue in tests, but we
        // can confirm the request succeeds (202) when a query_hint is supplied,
        // meaning the handler accepted and forwarded the payload.
        let app = make_app(Some("tok")).await;

        let req = TestRequest::post()
            .uri("/trigger/post")
            .insert_header(("Authorization", "Bearer tok"))
            .insert_header(("Content-Type", "application/json"))
            .set_payload(r#"{"query_hint":"lisbon"}"#)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert!(body["run_id"].as_str().is_some());
    }

    #[actix_web::test]
    async fn trigger_post_returns_409_when_run_in_flight() {
        let (app, pool) = make_app_with_pool(Some("tok")).await;

        // Pre-insert a queued run to simulate an in-flight run.
        let existing_id = "01EXISTING000000000000000000";
        run_insert(&pool, existing_id, Utc::now().timestamp())
            .await
            .unwrap();

        let req = TestRequest::post()
            .uri("/trigger/post")
            .insert_header(("Authorization", "Bearer tok"))
            .insert_header(("Content-Type", "application/json"))
            .set_payload("{}")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 409, "expected 409 Conflict");

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["run_id"].as_str().unwrap(), existing_id);
        assert_eq!(body["reason"].as_str().unwrap(), "run_already_in_flight");
        assert!(body["status_url"].as_str().unwrap().contains(existing_id));
    }

    #[actix_web::test]
    async fn trigger_post_returns_400_for_malformed_json() {
        let (app, _pool) = make_app_with_pool(Some("tok")).await;
        let req = TestRequest::post()
            .uri("/trigger/post")
            .insert_header(("Authorization", "Bearer tok"))
            .insert_header(("Content-Type", "application/json"))
            .set_payload("{not valid json")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400, "malformed JSON body should yield 400");
    }

    #[actix_web::test]
    async fn trigger_post_returns_401_without_bearer() {
        let app = make_app(Some("tok")).await;

        let req = TestRequest::post()
            .uri("/trigger/post")
            .insert_header(("Content-Type", "application/json"))
            .set_payload("{}")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    // -----------------------------------------------------------------------
    // GET /trigger/status/{run_id}
    // -----------------------------------------------------------------------

    #[actix_web::test]
    async fn trigger_status_returns_succeeded_run() {
        let (app, pool) = make_app_with_pool(Some("tok")).await;

        // Pre-insert a succeeded run.
        let run_id = "01SUCC000000000000000000000";
        run_insert(&pool, run_id, 1_700_000_000).await.unwrap();
        {
            let mut tx = pool.begin().await.unwrap();
            crate::storage::repo::run_mark_succeeded_in_tx(
                &mut tx,
                &crate::storage::repo::RunSuccessFields {
                    run_id,
                    finished_at: 1_700_000_100,
                    query_used: "mountain",
                    candidates_returned: 10,
                    candidates_after_filter: 3,
                    selected_asset_id: "asset-xyz",
                    caption: "A mountain view.",
                    buffer_post_id: "buf_post_abc",
                    duration_ms: 5000,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        let req = TestRequest::get()
            .uri(&format!("/trigger/status/{run_id}"))
            .insert_header(("Authorization", "Bearer tok"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["run_id"].as_str().unwrap(), run_id);
        assert_eq!(body["status"].as_str().unwrap(), "succeeded");
        assert_eq!(body["buffer_post_id"].as_str().unwrap(), "buf_post_abc");
        assert_eq!(body["caption"].as_str().unwrap(), "A mountain view.");
        assert_eq!(body["immich_asset_id"].as_str().unwrap(), "asset-xyz");
        // On succeeded, error should be absent
        assert!(body.get("error").is_none() || body["error"].is_null());
    }

    #[actix_web::test]
    async fn trigger_status_returns_failed_run() {
        let (app, pool) = make_app_with_pool(Some("tok")).await;

        let run_id = "01FAIL000000000000000000000";
        run_insert(&pool, run_id, 1_700_000_000).await.unwrap();
        run_mark_running(&pool, run_id).await.unwrap();
        run_mark_failed(&pool, run_id, "immich timeout", 3000)
            .await
            .unwrap();

        let req = TestRequest::get()
            .uri(&format!("/trigger/status/{run_id}"))
            .insert_header(("Authorization", "Bearer tok"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["status"].as_str().unwrap(), "failed");
        assert_eq!(body["error"].as_str().unwrap(), "immich timeout");
        // Success fields should be absent
        assert!(body.get("buffer_post_id").is_none() || body["buffer_post_id"].is_null());
    }

    #[actix_web::test]
    async fn trigger_status_returns_404_for_unknown_run() {
        let app = make_app(Some("tok")).await;

        let req = TestRequest::get()
            .uri("/trigger/status/01UNKNOWN0000000000000000000")
            .insert_header(("Authorization", "Bearer tok"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["error"].as_str().unwrap(), "run_not_found");
    }

    #[actix_web::test]
    async fn trigger_status_sets_cache_control_no_store() {
        let (app, pool) = make_app_with_pool(Some("tok")).await;

        let run_id = "01CACHE00000000000000000000";
        run_insert(&pool, run_id, 1_700_000_000).await.unwrap();

        let req = TestRequest::get()
            .uri(&format!("/trigger/status/{run_id}"))
            .insert_header(("Authorization", "Bearer tok"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        let cache_control = resp
            .headers()
            .get("Cache-Control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cache_control, "no-store");
    }
}
