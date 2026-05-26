//! Buffer GraphQL mutations.
//!
//! # Design notes
//!
//! ## Response parsing strategy
//!
//! Buffer returns a standard GraphQL envelope: `{ "data": { ... }, "errors": [...] }`.
//! The `createPost` field resolves to a union type with two branches:
//!
//! - `PostActionSuccess { post { id } }`
//! - `MutationError { message }`
//!
//! We use a `#[serde(tag = "__typename")]` enum to deserialise the union cleanly.
//! This requires the mutation to request `__typename` on `createPost` alongside the
//! inline fragments — which is included in `MUTATION` below.
//!
//! ## Asset format
//!
//! The mutation uses the NEW `[AssetInput!]` array format, effective 2026-05-25.
//! The legacy `assets: { images: [...] }` object shape stops working on that date.
//! Each array element is `{ "image": { "url": "<public-url>" } }`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::buffer::client::BufferClient;

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Successful Buffer post scheduling result.
#[derive(Debug, Clone)]
pub struct ScheduledPost {
    pub post_id: String,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while scheduling a Buffer post.
#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    /// Transport-level error from `reqwest`.
    #[error("buffer http error: {0}")]
    Http(#[from] reqwest::Error),

    /// Buffer returned a non-2xx HTTP status.
    #[error("buffer returned HTTP {status}: {body}")]
    Status { status: u16, body: String },

    /// Buffer returned 200 OK but the GraphQL union resolved to `MutationError`.
    /// Also used for top-level GraphQL `errors` arrays.
    #[error("buffer mutation error: {message}")]
    Mutation { message: String },

    /// Response was 200 OK but the JSON shape was not what we expected.
    #[error("could not parse buffer response: {0}")]
    Parse(String),
}

// ---------------------------------------------------------------------------
// GraphQL mutation
// ---------------------------------------------------------------------------

/// The `createPost` mutation.
///
/// We request `__typename` so the typed `CreatePostResult` enum can be
/// deserialised via `#[serde(tag = "__typename")]`.
const MUTATION: &str = r#"
mutation CreatePost($input: CreatePostInput!) {
  createPost(input: $input) {
    __typename
    ... on PostActionSuccess {
      post { id }
    }
    ... on MutationError {
      message
    }
    ... on UnexpectedError {
      message
    }
  }
}
"#;

// ---------------------------------------------------------------------------
// Serialisation types for the request
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GraphQlRequest<'a, V: Serialize> {
    query: &'a str,
    variables: V,
}

#[derive(Serialize)]
struct CreatePostVariables<'a> {
    input: CreatePostInput<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreatePostInput<'a> {
    text: &'a str,
    channel_id: &'a str,
    scheduling_type: &'static str,
    mode: &'static str,
    /// New [AssetInput!] array format (effective 2026-05-25).
    assets: Vec<AssetInput<'a>>,
    /// Instagram requires a post type ("post", "story", or "reel") via
    /// `metadata.instagram.type`. We only publish standard feed posts, so
    /// this is hard-coded to `"post"`.
    metadata: PostMetadata,
}

#[derive(Serialize)]
struct AssetInput<'a> {
    image: ImageAsset<'a>,
}

#[derive(Serialize)]
struct ImageAsset<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct PostMetadata {
    instagram: InstagramPostMetadata,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstagramPostMetadata {
    #[serde(rename = "type")]
    post_type: &'static str,
    should_share_to_feed: bool,
}

// ---------------------------------------------------------------------------
// Deserialisation types for the response
// ---------------------------------------------------------------------------

/// Top-level GraphQL envelope.
#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<Value>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

/// The union returned by `createPost`, discriminated by `__typename`.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum CreatePostResult {
    PostActionSuccess { post: PostResult },
    MutationError { message: String },
    UnexpectedError { message: String },
}

#[derive(Deserialize)]
struct PostResult {
    id: String,
}

// ---------------------------------------------------------------------------
// schedule_post
// ---------------------------------------------------------------------------

/// Schedule an Instagram post on the configured Buffer channel.
///
/// `image_url` must be a public URL that Buffer can fetch (e.g. the `/pic/<uuid>.jpg`
/// endpoint implemented in Task 6).
///
/// `caption_text` is the FINAL text Buffer will publish — Task 5 is responsible
/// for assembling the OpenAI caption + location suffix + hashtags before calling
/// this function.
///
/// The post is scheduled using `schedulingType: automatic` and `mode: addToQueue`,
/// which places it in the next available queue slot.
#[tracing::instrument(
    name = "buffer.create_post",
    skip(client, caption_text),
    fields(
        channel_id = %channel_id,
        image_url = %image_url,           // public-by-design URL; OK to log
        buffer_post_id = tracing::field::Empty,
        http_status = tracing::field::Empty,
    )
)]
pub async fn schedule_post(
    client: &BufferClient,
    channel_id: &str,
    image_url: &str,
    caption_text: &str,
) -> Result<ScheduledPost, BufferError> {
    let body = GraphQlRequest {
        query: MUTATION,
        variables: CreatePostVariables {
            input: CreatePostInput {
                text: caption_text,
                channel_id,
                scheduling_type: "automatic",
                mode: "addToQueue",
                assets: vec![AssetInput {
                    image: ImageAsset { url: image_url },
                }],
                metadata: PostMetadata {
                    instagram: InstagramPostMetadata {
                        post_type: "post",
                        should_share_to_feed: true,
                    },
                },
            },
        },
    };

    let response = client
        .http
        .post(&client.base_url)
        .bearer_auth(&client.api_key)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    tracing::Span::current().record("http_status", status.as_u16());

    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        return Err(BufferError::Status {
            status: status.as_u16(),
            body: body_text,
        });
    }

    let envelope: GraphQlResponse = response
        .json()
        .await
        .map_err(|e| BufferError::Parse(format!("failed to deserialise response envelope: {e}")))?;

    // 1. Top-level GraphQL errors take priority.
    if let Some(errors) = envelope.errors
        && !errors.is_empty()
    {
        let message = errors
            .into_iter()
            .next()
            .expect("non-empty by guard above")
            .message;
        return Err(BufferError::Mutation { message });
    }

    // 2. Navigate to data.createPost and deserialise the union.
    let data = envelope
        .data
        .ok_or_else(|| BufferError::Parse("response missing 'data' field".into()))?;

    tracing::info!(data = ?data, "GraphQL response data");

    let create_post = data
        .get("createPost")
        .ok_or_else(|| BufferError::Parse("'data.createPost' missing in response".into()))?;

    let result: CreatePostResult = serde_json::from_value(create_post.clone())
        .map_err(|e| BufferError::Parse(format!("failed to deserialise createPost result: {e}")))?;

    match result {
        CreatePostResult::PostActionSuccess { post } => {
            tracing::Span::current().record("buffer_post_id", &post.id);
            Ok(ScheduledPost { post_id: post.id })
        }
        CreatePostResult::MutationError { message }
        | CreatePostResult::UnexpectedError { message } => Err(BufferError::Mutation { message }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::buffer::client::BufferClient;

    fn make_client(server_uri: &str) -> BufferClient {
        BufferClient::with_base_url("test-key", server_uri)
    }

    // -----------------------------------------------------------------------
    // schedule_post_returns_post_id_on_success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn schedule_post_returns_post_id_on_success() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_partial_json(json!({
                "variables": {
                    "input": {
                        "text": "A beautiful view.",
                        "channelId": "chan_abc",
                        "schedulingType": "automatic",
                        "mode": "addToQueue",
                        "assets": [
                            { "image": { "url": "https://example.com/photo.jpg" } }
                        ],
                        "metadata": {
                            "instagram": {
                                "type": "post",
                                "shouldShareToFeed": true
                            }
                        }
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "createPost": {
                        "__typename": "PostActionSuccess",
                        "post": { "id": "buf_post_123" }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let result = schedule_post(
            &client,
            "chan_abc",
            "https://example.com/photo.jpg",
            "A beautiful view.",
        )
        .await
        .unwrap();

        assert_eq!(result.post_id, "buf_post_123");
    }

    // -----------------------------------------------------------------------
    // schedule_post_returns_mutation_error_branch
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn schedule_post_returns_mutation_error_branch() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "createPost": {
                        "__typename": "MutationError",
                        "message": "Channel not authorised"
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let err = schedule_post(
            &client,
            "chan_abc",
            "https://example.com/photo.jpg",
            "caption",
        )
        .await
        .unwrap_err();

        match err {
            BufferError::Mutation { message } => {
                assert!(
                    message.contains("Channel not authorised"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected BufferError::Mutation, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // schedule_post_returns_status_error_on_401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn schedule_post_returns_status_error_on_401() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let err = schedule_post(
            &client,
            "chan_abc",
            "https://example.com/photo.jpg",
            "caption",
        )
        .await
        .unwrap_err();

        match err {
            BufferError::Status { status, .. } => {
                assert_eq!(status, 401);
            }
            other => panic!("expected BufferError::Status, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // schedule_post_returns_mutation_error_on_top_level_graphql_errors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn schedule_post_returns_mutation_error_on_top_level_graphql_errors() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "Bad input" }],
                "data": null
            })))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let err = schedule_post(
            &client,
            "chan_abc",
            "https://example.com/photo.jpg",
            "caption",
        )
        .await
        .unwrap_err();

        match err {
            BufferError::Mutation { message } => {
                assert!(
                    message.contains("Bad input"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected BufferError::Mutation, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // schedule_post_request_uses_new_asset_input_format
    //
    // Regression guard: verifies the request body uses the new [AssetInput!]
    // array format ({ "image": { "url": "..." } }) and NOT the legacy
    // { "images": [...] } object shape that stops working on 2026-05-25.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn schedule_post_request_uses_new_asset_input_format() {
        let server = MockServer::start().await;

        // Match on the new array format. `body_partial_json` will fail if the
        // request body does not contain this exact nested structure.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "variables": {
                    "input": {
                        "assets": [
                            { "image": { "url": "https://example.com/foo.jpg" } }
                        ]
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "createPost": {
                        "__typename": "PostActionSuccess",
                        "post": { "id": "buf_post_456" }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let result = schedule_post(
            &client,
            "chan_xyz",
            "https://example.com/foo.jpg",
            "Scenic coastline.",
        )
        .await
        .unwrap();

        // If we had used the legacy format, the mock would not have matched and
        // we'd get a 404 / connection refused, causing the test to fail.
        assert_eq!(result.post_id, "buf_post_456");
    }
}
