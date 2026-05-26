//! `GET /pic/{uuid}.jpg` — unauthenticated thumbnail proxy.
//!
//! Buffer fetches this URL when scheduling an Instagram post.  The route:
//! 1. Looks up the `pending_media` row by UUID.
//! 2. Checks expiry (`expires_at` must be in the future).
//! 3. Fetches the JPEG bytes from Immich via [`ImmichClient::fetch_thumbnail`].
//! 4. Returns the bytes with `Content-Type: image/jpeg`, `Cache-Control`, and
//!    a weak `ETag`.
//!
//! No Bearer auth — Buffer has no mechanism to send it.

use actix_web::{HttpResponse, web};
use chrono::Utc;
use sqlx::SqlitePool;

use crate::error::AppError;
use crate::immich::client::ImmichClient;
use crate::storage::repo::pending_media_lookup;

/// `GET /pic/{uuid}.jpg`
pub async fn serve_pic(
    pool: web::Data<SqlitePool>,
    immich: web::Data<ImmichClient>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let uuid = path.into_inner();

    // Basic format validation: UUID v4 is 36 chars (with hyphens) or 32 hex chars.
    // Reject obviously malformed values so we don't hit the DB with garbage.
    let valid_len = uuid.len() == 36 || uuid.len() == 32;
    if !valid_len {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" })));
    }

    // Step 2: DB lookup.
    let row = match pending_media_lookup(pool.get_ref(), &uuid).await? {
        Some(r) => r,
        None => {
            return Ok(HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" })));
        }
    };

    // Step 3: expiry check.
    let now_epoch = Utc::now().timestamp();
    if row.expires_at < now_epoch {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" })));
    }

    // Step 4: fetch from Immich.
    let jpeg_bytes = match immich.fetch_thumbnail(&row.immich_asset_id).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                asset_id = %row.immich_asset_id,
                error = %e,
                "immich thumbnail fetch failed; returning 502"
            );
            return Ok(HttpResponse::BadGateway()
                .json(serde_json::json!({ "error": "upstream_unavailable" })));
        }
    };

    // Step 5: build response.
    let etag = format!("W/\"{}\"", row.immich_asset_id);
    Ok(HttpResponse::Ok()
        .insert_header(("Content-Type", "image/jpeg"))
        .insert_header(("Cache-Control", "public, max-age=300"))
        .insert_header(("ETag", etag))
        .body(jpeg_bytes))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immich::client::ImmichClient;
    use crate::storage::db::test_pool;
    use crate::storage::repo::pending_media_insert;
    use actix_web::test::{self, TestRequest};
    use actix_web::{App, web};
    use chrono::Utc;
    use wiremock::matchers::{method, path as wm_path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Returns a test app wired to the given Immich base URL.
    async fn make_pic_app(
        immich_base: String,
    ) -> (
        impl actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
            Error = actix_web::Error,
        >,
        sqlx::SqlitePool,
    ) {
        let pool = test_pool().await;
        let immich = ImmichClient::new(immich_base.as_str(), "test-key");

        let pool_data = web::Data::new(pool.clone());
        let immich_data = web::Data::new(immich);

        let svc = test::init_service(
            App::new()
                .app_data(pool_data)
                .app_data(immich_data)
                .route("/pic/{uuid}.jpg", web::get().to(serve_pic)),
        )
        .await;

        (svc, pool)
    }

    #[actix_web::test]
    async fn pic_returns_jpeg_bytes_for_valid_uuid() {
        let server = MockServer::start().await;
        let fake_jpeg = b"\xff\xd8\xff\xe0fake jpeg bytes".to_vec();
        let asset_id = "asset-uuid-pic-1";

        Mock::given(method("GET"))
            .and(wm_path(format!("/api/assets/{asset_id}/thumbnail")))
            .and(query_param("size", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_jpeg.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let (app, pool) = make_pic_app(server.uri()).await;

        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let expires_at = Utc::now().timestamp() + 3600;
        pending_media_insert(&pool, uuid, asset_id, expires_at)
            .await
            .unwrap();

        let req = TestRequest::get()
            .uri(&format!("/pic/{uuid}.jpg"))
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), 200);

        let content_type = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "image/jpeg");

        let cache_ctrl = resp
            .headers()
            .get("Cache-Control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cache_ctrl, "public, max-age=300");

        let etag = resp
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(etag, format!("W/\"{asset_id}\""));

        let body = test::read_body(resp).await;
        assert_eq!(body.as_ref(), fake_jpeg.as_slice());
    }

    #[actix_web::test]
    async fn pic_returns_404_for_unknown_uuid() {
        let server = MockServer::start().await;
        let (app, _pool) = make_pic_app(server.uri()).await;

        let req = TestRequest::get()
            .uri("/pic/550e8400-e29b-41d4-a716-446655440001.jpg")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn pic_returns_404_for_expired_uuid() {
        let server = MockServer::start().await;
        let (app, pool) = make_pic_app(server.uri()).await;

        let uuid = "550e8400-e29b-41d4-a716-446655440002";
        let expires_at = Utc::now().timestamp() - 1; // expired
        pending_media_insert(&pool, uuid, "asset-expired", expires_at)
            .await
            .unwrap();

        let req = TestRequest::get()
            .uri(&format!("/pic/{uuid}.jpg"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn pic_returns_502_when_immich_fails() {
        let server = MockServer::start().await;
        let asset_id = "asset-502";

        Mock::given(method("GET"))
            .and(wm_path(format!("/api/assets/{asset_id}/thumbnail")))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&server)
            .await;

        let (app, pool) = make_pic_app(server.uri()).await;

        let uuid = "550e8400-e29b-41d4-a716-446655440003";
        let expires_at = Utc::now().timestamp() + 3600;
        pending_media_insert(&pool, uuid, asset_id, expires_at)
            .await
            .unwrap();

        let req = TestRequest::get()
            .uri(&format!("/pic/{uuid}.jpg"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 502);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["error"].as_str().unwrap(), "upstream_unavailable");
    }

    #[actix_web::test]
    async fn pic_does_not_require_bearer_auth() {
        // GET without any Authorization header must return 404 (not 401) for
        // an unknown UUID — proves the route has no auth guard.
        let server = MockServer::start().await;
        let (app, _pool) = make_pic_app(server.uri()).await;

        let req = TestRequest::get()
            .uri("/pic/550e8400-e29b-41d4-a716-446655440004.jpg")
            // No Authorization header
            .to_request();
        let resp = test::call_service(&app, req).await;
        // 404 = route reached, no 401 = no auth check
        assert_eq!(resp.status(), 404);
    }
}
