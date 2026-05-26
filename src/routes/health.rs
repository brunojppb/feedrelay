use actix_web::{HttpResponse, web};
use serde_json::json;
use sqlx::SqlitePool;

/// `GET /management/health`
///
/// Returns `200 OK` with `{ "db": "ok", "worker": "ok" }` when the database is
/// reachable, or `503 Service Unavailable` with `{ "db": "error", "worker": "ok" }`
/// on database failure.
///
/// ## Worker health
///
/// The worker runs in the same process.  If it crashes, the process exits and the
/// health endpoint stops responding entirely — so `"worker": "ok"` is always the
/// correct response when this endpoint is reachable.  A production deployment should
/// use a process supervisor (e.g. Docker restart policy) to detect the absence of a
/// responding health endpoint.
///
/// No authentication required.
pub async fn health(pool: web::Data<SqlitePool>) -> HttpResponse {
    match sqlx::query("SELECT 1").execute(pool.get_ref()).await {
        Ok(_) => {
            tracing::debug!("health check: db ok");
            HttpResponse::Ok().json(json!({ "db": "ok", "worker": "ok" }))
        }
        Err(err) => {
            tracing::error!(error = %err, "health check: db error");
            HttpResponse::ServiceUnavailable().json(json!({ "db": "error", "worker": "ok" }))
        }
    }
}
