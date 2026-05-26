//! Bearer-token middleware for the `/trigger/*` scope.
//!
//! Uses actix-web 4's `from_fn` middleware so the implementation is a single
//! async function rather than the `Transform` + `Service` trait pair.
//!
//! The expected token lives in [`AuthConfig`], which is registered as
//! `web::Data<AuthConfig>` on the `App`.  The middleware reads it from the
//! request's `app_data`.
//!
//! # Timing safety
//!
//! Token comparison uses [`subtle::ConstantTimeEq`] so the comparison time
//! does not leak information about how many bytes matched.
//! `ct_eq` on `[u8]` handles differing lengths correctly — no manual
//! length pre-check is needed or desired.

use actix_web::middleware::Next;
use actix_web::{
    Error, HttpResponse,
    body::{EitherBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    web,
};
use subtle::ConstantTimeEq;

/// App state injected as `web::Data<AuthConfig>`.
pub struct AuthConfig {
    /// The token callers must send in `Authorization: Bearer <token>`.
    ///
    /// `None` means no token is configured; every request is rejected.
    pub expected_token: Option<String>,
}

/// `from_fn`-compatible middleware.
///
/// Mounted on the `/trigger` scope only.  Public routes (`/management/health`,
/// `/pic/{uuid}.jpg`) are outside this scope and never hit this function.
pub async fn bearer_auth_middleware(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<EitherBody<impl MessageBody>>, Error> {
    let auth_cfg = req
        .app_data::<web::Data<AuthConfig>>()
        .expect("AuthConfig must be registered with App::app_data");

    let expected = match &auth_cfg.expected_token {
        Some(t) => t.as_bytes(),
        None => {
            // No token configured — always reject.
            return Ok(req.into_response(
                HttpResponse::Unauthorized()
                    .json(serde_json::json!({ "error": "unauthorized" }))
                    .map_into_right_body(),
            ));
        }
    };

    // Extract the raw header value.
    let provided = req
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let authed = match provided {
        Some(token) => bool::from(token.as_bytes().ct_eq(expected)),
        None => false,
    };

    if authed {
        next.call(req).await.map(|r| r.map_into_left_body())
    } else {
        Ok(req.into_response(
            HttpResponse::Unauthorized()
                .json(serde_json::json!({ "error": "unauthorized" }))
                .map_into_right_body(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        App, HttpResponse,
        middleware::from_fn,
        test::{self, TestRequest},
        web,
    };

    async fn dummy_handler() -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({ "ok": true }))
    }

    fn make_app(
        token: Option<&str>,
    ) -> actix_web::App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        let auth_cfg = web::Data::new(AuthConfig {
            expected_token: token.map(|s| s.to_string()),
        });

        App::new().app_data(auth_cfg).service(
            web::scope("/trigger")
                .wrap(from_fn(bearer_auth_middleware))
                .route("/test", web::get().to(dummy_handler)),
        )
    }

    #[actix_web::test]
    async fn bearer_auth_passes_with_correct_token() {
        let app = test::init_service(make_app(Some("correct-token"))).await;
        let req = TestRequest::get()
            .uri("/trigger/test")
            .insert_header(("Authorization", "Bearer correct-token"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn bearer_auth_rejects_missing_header() {
        let app = test::init_service(make_app(Some("correct-token"))).await;
        let req = TestRequest::get().uri("/trigger/test").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn bearer_auth_rejects_wrong_token() {
        let app = test::init_service(make_app(Some("correct-token"))).await;
        let req = TestRequest::get()
            .uri("/trigger/test")
            .insert_header(("Authorization", "Bearer wrong-token"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn bearer_auth_rejects_when_no_token_configured() {
        let app = test::init_service(make_app(None)).await;
        let req = TestRequest::get()
            .uri("/trigger/test")
            .insert_header(("Authorization", "Bearer whatever"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["error"], "unauthorized");
    }
}
