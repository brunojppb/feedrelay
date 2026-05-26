//! Caption generation with cache-first lookup.
//!
//! Public API:
//! - [`get_or_generate_caption`] — orchestrates prompt rendering, DB lookup, and OpenAI call.
//! - [`Caption`] — the structured caption result.
//! - [`CaptionContext`] — EXIF metadata inputs for prompt rendering.
//! - [`CaptionError`] — error variants.
//! - [`OpenAiClient`] — re-exported for callers that need to construct the client.

pub mod openai;
pub mod prompt;

pub use openai::OpenAiClient;

use crate::storage::repo::{caption_insert, caption_lookup};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Inputs needed to render the caption prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionContext<'a> {
    pub city: Option<&'a str>,
    pub country: Option<&'a str>,
    pub date: Option<chrono::NaiveDate>,
}

/// The structured caption result returned by OpenAI and cached in SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caption {
    /// 1-2 sentence body — no location, no hashtags.
    pub text: String,
    /// Lowercase, no `#` prefix.  3-5 entries.
    pub hashtags: Vec<String>,
    /// Screen-reader description of the photo's visual contents.
    pub alt_text: String,
}

/// Errors that can occur during caption generation or cache access.
#[derive(Debug, Error)]
pub enum CaptionError {
    #[error("openai http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("openai returned HTTP {status}: {body}")]
    Status { status: u16, body: String },

    #[error("could not parse openai response: {0}")]
    Parse(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

// ---------------------------------------------------------------------------
// Cache key format
// ---------------------------------------------------------------------------

/// Compose the stable cache key from rendered system + user prompts.
///
/// **Format (do not change without invalidating the cache):**
/// ```text
/// system:
/// <system prompt text>
///
/// user:
/// <user prompt text>
/// ```
///
/// This format embeds both prompts verbatim so any template change produces a
/// new key automatically, avoiding stale cache hits after prompt edits.
fn compose_cache_key(system: &str, user: &str) -> String {
    format!("system:\n{system}\n\nuser:\n{user}")
}

// ---------------------------------------------------------------------------
// Public function
// ---------------------------------------------------------------------------

/// Cache-first caption generation.
///
/// # Flow
///
/// 1. `render_prompts(ctx)` → `RenderedPrompts { system, user }`
/// 2. Compose `rendered_prompt` (the cache key) via [`compose_cache_key`]
/// 3. `caption_lookup(pool, asset_id, &rendered_prompt)`:
///    - `Some(cached)` → log cache hit, return early
/// 4. `None` → log cache miss, call OpenAI
/// 5. `caption_insert(pool, asset_id, &rendered_prompt, &caption)`
/// 6. Return the fresh caption
#[tracing::instrument(
    name = "caption.lookup",
    skip(pool, openai, image_bytes, ctx),
    fields(asset_id = %asset_id)
)]
pub async fn get_or_generate_caption(
    pool: &sqlx::SqlitePool,
    openai: &OpenAiClient,
    asset_id: &str,
    image_bytes: &[u8],
    ctx: &CaptionContext<'_>,
) -> Result<Caption, CaptionError> {
    let rendered = prompt::render_prompts(ctx);
    let cache_key = compose_cache_key(&rendered.system, &rendered.user);

    match caption_lookup(pool, asset_id, &cache_key).await? {
        Some(cached) => {
            tracing::info!(cache_hit = true, "caption cache hit");
            return Ok(cached);
        }
        None => {
            tracing::info!(cache_hit = false, "caption cache miss — calling OpenAI");
        }
    }

    let caption = openai::generate(openai, &rendered.system, &rendered.user, image_bytes).await?;

    caption_insert(pool, asset_id, &cache_key, &caption).await?;

    Ok(caption)
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::db::test_pool;

    fn make_ctx<'a>(
        city: Option<&'a str>,
        country: Option<&'a str>,
        date: Option<NaiveDate>,
    ) -> CaptionContext<'a> {
        CaptionContext {
            city,
            country,
            date,
        }
    }

    /// Minimal valid Responses API JSON that our extractor can parse.
    fn mock_openai_response(caption: &str, hashtags: &[&str], alt_text: &str) -> serde_json::Value {
        let caption_json = json!({
            "caption": caption,
            "hashtags": hashtags,
            "alt_text": alt_text
        });
        json!({
            "id": "resp_test",
            "object": "response",
            "model": "gpt-5.4-mini",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [
                        {
                            "type": "output_text",
                            "text": caption_json.to_string()
                        }
                    ]
                }
            ]
        })
    }

    // -----------------------------------------------------------------------
    // caption_cache_hit_does_not_call_openai
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn caption_cache_hit_does_not_call_openai() {
        let pool = test_pool().await;

        // Pre-seed the cache directly via repo
        let ctx = make_ctx(
            Some("Lisbon"),
            Some("Portugal"),
            NaiveDate::from_ymd_opt(2024, 6, 1),
        );
        let rendered = prompt::render_prompts(&ctx);
        let key = compose_cache_key(&rendered.system, &rendered.user);

        let cached = Caption {
            text: "Sunrise over the water.".into(),
            hashtags: vec!["lisbon".into(), "morning".into()],
            alt_text: "Golden light on the Tagus.".into(),
        };
        crate::storage::repo::caption_insert(&pool, "asset-001", &key, &cached)
            .await
            .unwrap();

        // Set up a mock server that must NOT be called (expect 0 requests)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0) // assert no calls reach the server
            .mount(&server)
            .await;

        let client = OpenAiClient::with_base_url("sk-test", "gpt-5.4-mini", server.uri());

        let result = get_or_generate_caption(&pool, &client, "asset-001", b"fake-jpeg-bytes", &ctx)
            .await
            .unwrap();

        assert_eq!(result, cached);
    }

    // -----------------------------------------------------------------------
    // caption_cache_miss_calls_openai_and_persists
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn caption_cache_miss_calls_openai_and_persists() {
        let pool = test_pool().await;
        let server = MockServer::start().await;

        let response_body = mock_openai_response(
            "Afternoon light filters through the arched windows.",
            &["porto", "azulejos", "architecture"],
            "Colourful tiled facade on a sunny afternoon.",
        );

        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenAiClient::with_base_url("sk-test", "gpt-5.4-mini", server.uri());
        let ctx = make_ctx(
            Some("Porto"),
            Some("Portugal"),
            NaiveDate::from_ymd_opt(2024, 7, 20),
        );

        let result = get_or_generate_caption(&pool, &client, "asset-002", b"jpeg", &ctx)
            .await
            .unwrap();

        assert_eq!(
            result.text,
            "Afternoon light filters through the arched windows."
        );
        assert_eq!(result.hashtags, vec!["porto", "azulejos", "architecture"]);
        assert_eq!(
            result.alt_text,
            "Colourful tiled facade on a sunny afternoon."
        );

        // Verify it was persisted
        let rendered = prompt::render_prompts(&ctx);
        let key = compose_cache_key(&rendered.system, &rendered.user);
        let from_db = caption_lookup(&pool, "asset-002", &key).await.unwrap();
        assert_eq!(from_db, Some(result));
    }

    // -----------------------------------------------------------------------
    // openai_propagates_non_2xx_as_status_error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn openai_propagates_non_2xx_as_status_error() {
        let pool = test_pool().await;
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("{\"error\":{\"message\":\"Invalid API key\"}}"),
            )
            .mount(&server)
            .await;

        let client = OpenAiClient::with_base_url("sk-bad-key", "gpt-5.4-mini", server.uri());
        let ctx = make_ctx(Some("Rome"), Some("Italy"), None);

        let err = get_or_generate_caption(&pool, &client, "asset-003", b"jpeg", &ctx)
            .await
            .unwrap_err();

        assert!(
            matches!(err, CaptionError::Status { status: 401, .. }),
            "expected Status(401), got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // hashtags_round_trip_through_db
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn hashtags_round_trip_through_db() {
        let pool = test_pool().await;

        let caption = Caption {
            text: "The tiles tell a story.".into(),
            hashtags: vec!["lisbon".into(), "azulejos".into()],
            alt_text: "Blue and white tiles on a wall.".into(),
        };

        crate::storage::repo::caption_insert(&pool, "asset-ht", "prompt-key", &caption)
            .await
            .unwrap();

        let retrieved = crate::storage::repo::caption_lookup(&pool, "asset-ht", "prompt-key")
            .await
            .unwrap()
            .expect("expected a cached row");

        assert_eq!(retrieved.hashtags, vec!["lisbon", "azulejos"]);
        assert_eq!(retrieved, caption);
    }
}
