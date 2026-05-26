//! OpenAI Responses API client.
//!
//! # API shape (verified against openai-python SDK source, 2026-05-24)
//!
//! **Endpoint:** `POST /v1/responses`
//!
//! **Request body:**
//! ```json
//! {
//!   "model": "gpt-5.4-mini",
//!   "instructions": "<system prompt>",
//!   "input": [
//!     {
//!       "role": "user",
//!       "content": [
//!         { "type": "input_text", "text": "<user prompt>" },
//!         { "type": "input_image", "image_url": "data:image/jpeg;base64,...", "detail": "low" }
//!       ]
//!     }
//!   ],
//!   "text": {
//!     "format": {
//!       "type": "json_schema",
//!       "name": "caption_output",
//!       "strict": true,
//!       "schema": { ... }
//!     }
//!   }
//! }
//! ```
//!
//! **Response body** (relevant fields):
//! ```json
//! {
//!   "output": [
//!     {
//!       "type": "message",
//!       "role": "assistant",
//!       "content": [
//!         { "type": "output_text", "text": "{\"caption\":\"...\",\"hashtags\":[...],\"alt_text\":\"...\"}" }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Sources consulted:
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_create_params.py>
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_input_image_content_param.py>
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_input_text_content_param.py>
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_format_text_json_schema_config_param.py>
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_output_message.py>
//! - <https://raw.githubusercontent.com/openai/openai-python/main/src/openai/types/responses/response_output_text.py>
//!
//! # TODOs for live-test verification
//!
//! - TODO(live-test): Confirm `instructions` is the correct top-level field for the
//!   system prompt (vs. a `{ role: "system" }` message in `input`). The SDK type
//!   `ResponseCreateParams` has `instructions: Optional[str]` at the top level; that
//!   is what we use here. If the model ignores it, move the system text into a
//!   `{ role: "system", content: [{ type: "input_text", text: ... }] }` entry at
//!   the start of the `input` array instead.
//!
//! - TODO(live-test): Confirm the `detail` field on `input_image` is accepted as a
//!   sibling of `image_url` (not nested inside it). The SDK's
//!   `ResponseInputImageContentParam` shows `detail` and `image_url` as top-level
//!   fields on the content object, which is what we send.
//!
//! - TODO(live-test): If the model returns a refusal (content[0].type == "refusal")
//!   instead of "output_text", surface it as `CaptionError::Parse` with the refusal
//!   text. Current code returns a generic parse error.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::caption::{Caption, CaptionError};

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// OpenAI Responses API client.  Clone-cheap (inner `reqwest::Client` is Arc-backed).
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    pub(crate) http: reqwest::Client,
    pub(crate) api_key: String,
    pub(crate) model: String,
    /// Base URL without trailing slash.  Default: `"https://api.openai.com"`.
    pub(crate) base_url: String,
}

impl OpenAiClient {
    /// Create a new client targeting the default OpenAI endpoint.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_base_url(api_key, model, "https://api.openai.com")
    }

    /// Create a client with a custom base URL — used in wiremock-driven tests.
    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::default(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// The JSON object the model must return (enforced via Structured Outputs).
#[derive(Debug, Deserialize)]
struct CaptionOutput {
    caption: String,
    hashtags: Vec<String>,
    alt_text: String,
}

// ---------------------------------------------------------------------------
// generate()
// ---------------------------------------------------------------------------

/// Call the Responses API and return a parsed [`Caption`].
///
/// - Encodes `image_jpeg` as a base64 data URL and attaches it at `detail: "low"`.
/// - Uses Structured Outputs (`text.format`) to enforce the caption JSON schema.
/// - Returns `CaptionError::Status` on non-2xx, `CaptionError::Parse` on bad JSON,
///   `CaptionError::Http` on transport errors.
#[tracing::instrument(
    name = "openai.caption",
    skip_all,
    fields(input_tokens = tracing::field::Empty, output_tokens = tracing::field::Empty)
)]
pub async fn generate(
    client: &OpenAiClient,
    system: &str,
    user: &str,
    image_jpeg: &[u8],
) -> Result<Caption, CaptionError> {
    let data_url = format!("data:image/jpeg;base64,{}", BASE64.encode(image_jpeg));

    // Build the JSON schema for Structured Outputs.
    // We use a flat object with three required fields; `additionalProperties: false`
    // is required when `strict: true`.
    let schema = json!({
        "type": "object",
        "properties": {
            "caption":   { "type": "string" },
            "hashtags":  { "type": "array", "items": { "type": "string" } },
            "alt_text":  { "type": "string" }
        },
        "required": ["caption", "hashtags", "alt_text"],
        "additionalProperties": false
    });

    let body = json!({
        "model": client.model,
        "instructions": system,
        "input": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": user
                    },
                    {
                        "type": "input_image",
                        "image_url": data_url,
                        "detail": "low"
                    }
                ]
            }
        ],
        "text": {
            "format": {
                "type": "json_schema",
                "name": "caption_output",
                "strict": true,
                "schema": schema
            }
        }
    });

    let url = format!("{}/v1/responses", client.base_url);

    let response = client
        .http
        .post(&url)
        .bearer_auth(&client.api_key)
        .json(&body)
        .send()
        .await?;

    let status = response.status();

    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(CaptionError::Status {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let raw: Value = response.json().await?;

    if let Some(usage) = raw.get("usage") {
        let input = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        tracing::Span::current()
            .record("input_tokens", input)
            .record("output_tokens", output);
    }

    extract_caption(raw)
}

/// Walk the Responses API response body and deserialise the `CaptionOutput`.
///
/// Expected path: `output[0].content[0].text` where
/// `output[0].type == "message"` and `output[0].content[0].type == "output_text"`.
fn extract_caption(raw: Value) -> Result<Caption, CaptionError> {
    let output = raw
        .get("output")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CaptionError::Parse("missing 'output' array in response".into()))?;

    // Find the first item of type "message" (skip reasoning items etc.)
    let message = output
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
        .ok_or_else(|| CaptionError::Parse("no 'message' item in output array".into()))?;

    let content = message
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CaptionError::Parse("missing 'content' array on message".into()))?;

    let text_content = content
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("output_text"))
        .ok_or_else(|| CaptionError::Parse("no 'output_text' item in message content".into()))?;

    let json_str = text_content
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CaptionError::Parse("'text' field missing or not a string".into()))?;

    let output: CaptionOutput = serde_json::from_str(json_str)
        .map_err(|e| CaptionError::Parse(format!("failed to parse caption JSON: {e}")))?;

    Ok(Caption {
        text: output.caption,
        hashtags: output
            .hashtags
            .into_iter()
            .map(|h| h.trim_start_matches('#').to_lowercase())
            .filter(|h| !h.is_empty())
            .collect(),
        alt_text: output.alt_text,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_caption_happy_path() {
        let raw = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "{\"caption\":\"Quiet morning by the river.\",\"hashtags\":[\"lisbon\",\"portugal\",\"riverside\"],\"alt_text\":\"A calm river at dawn with reflections.\"}"
                        }
                    ]
                }
            ]
        });

        let caption = extract_caption(raw).unwrap();
        assert_eq!(caption.text, "Quiet morning by the river.");
        assert_eq!(caption.hashtags, vec!["lisbon", "portugal", "riverside"]);
        assert_eq!(caption.alt_text, "A calm river at dawn with reflections.");
    }

    #[test]
    fn extract_caption_skips_non_message_items() {
        // Responses API can return reasoning items before the message
        let raw = json!({
            "output": [
                { "type": "reasoning", "content": [] },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "{\"caption\":\"Narrow streets of the old town.\",\"hashtags\":[\"oldtown\",\"travel\"],\"alt_text\":\"Cobblestoned alley.\"}"
                        }
                    ]
                }
            ]
        });

        let caption = extract_caption(raw).unwrap();
        assert_eq!(caption.text, "Narrow streets of the old town.");
    }

    #[test]
    fn extract_caption_missing_output_array_returns_parse_error() {
        let raw = json!({ "id": "resp_123" });
        let err = extract_caption(raw).unwrap_err();
        assert!(matches!(err, CaptionError::Parse(_)));
    }

    #[test]
    fn extract_caption_bad_json_in_text_returns_parse_error() {
        let raw = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "not-valid-json" }
                    ]
                }
            ]
        });
        let err = extract_caption(raw).unwrap_err();
        assert!(matches!(err, CaptionError::Parse(_)));
    }

    #[test]
    fn extract_caption_sanitises_hashtags() {
        let raw = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "{\"caption\":\"Test.\",\"hashtags\":[\"#Lisbon\",\"#PARQUE\",\"wanderlust\"],\"alt_text\":\"A test.\"}"
                        }
                    ]
                }
            ]
        });

        let caption = extract_caption(raw).unwrap();
        assert_eq!(caption.hashtags, vec!["lisbon", "parque", "wanderlust"]);
    }
}
