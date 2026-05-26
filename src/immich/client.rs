//! Immich HTTP client.
//!
//! Thin wrapper around `reqwest::Client` that holds the base URL and API key.
//! The API key is sent as the `x-api-key` header on every request (Immich does
//! NOT use Bearer auth).
//!
//! Individual request functions (e.g. `search_smart`, `fetch_faces`) live in
//! their own modules and accept `&ImmichClient` as their first argument.

use crate::immich::search::ImmichError;

/// Immich API client.  Clone-cheap (inner `reqwest::Client` is `Arc`-backed).
#[derive(Debug, Clone)]
pub struct ImmichClient {
    pub(crate) http: reqwest::Client,
    /// Base URL without trailing slash, e.g. `"https://immich.example.com"`.
    pub(crate) base_url: String,
    pub(crate) api_key: String,
}

impl ImmichClient {
    /// Create a new client.
    ///
    /// `base_url` must not have a trailing slash.
    /// `api_key` is the Immich API key from the admin UI.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::default(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
        }
    }

    /// Full URL for a given API path (path must start with `/`).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// `GET /api/assets/{id}/thumbnail?size=preview`
    ///
    /// Returns the raw JPEG bytes of the preview thumbnail.  Immich serves JPEG
    /// by default when the preview format is configured in the admin UI (which is
    /// the standard default).  We do NOT probe at runtime — we trust the stream.
    ///
    /// Response is buffered in full. Immich preview thumbnails are expected to be
    /// <2 MB; no size cap is applied.
    ///
    /// # Errors
    ///
    /// Returns [`ImmichError::Http`] for transport failures and
    /// [`ImmichError::Status`] for non-2xx responses.
    pub async fn fetch_thumbnail(&self, asset_id: &str) -> Result<Vec<u8>, ImmichError> {
        let url = self.url(&format!("/api/assets/{asset_id}/thumbnail"));

        let response = self
            .http
            .get(&url)
            .header("x-api-key", &self.api_key)
            .query(&[("size", "preview")])
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<body read error: {e}>"));
            return Err(ImmichError::Status {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_thumbnail_returns_jpeg_bytes() {
        let server = MockServer::start().await;
        let fake_jpeg = b"\xff\xd8\xff\xe0fake jpeg bytes".to_vec();

        Mock::given(method("GET"))
            .and(path("/api/assets/asset-uuid-1/thumbnail"))
            .and(query_param("size", "preview"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_jpeg.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "test-key");
        let bytes = client
            .fetch_thumbnail("asset-uuid-1")
            .await
            .expect("fetch_thumbnail failed");

        assert_eq!(bytes, fake_jpeg);
    }

    #[tokio::test]
    async fn fetch_thumbnail_propagates_non_2xx_as_status_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/assets/bad-id/thumbnail"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = ImmichClient::new(server.uri(), "test-key");
        let err = client
            .fetch_thumbnail("bad-id")
            .await
            .expect_err("expected an error for 404");

        assert!(
            matches!(err, ImmichError::Status { status: 404, .. }),
            "expected Status(404), got: {err:?}"
        );
    }
}
