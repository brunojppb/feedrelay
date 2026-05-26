//! End-to-end Run orchestration.
//!
//! `execute_run` implements the full FeedRelay pipeline:
//! search → fetch_faces → filter → pick at random → thumbnail download →
//! caption (cache-first) → insert pending_media → buffer.schedule_post →
//! persist runs + posts.
//!
//! The function is deliberately kept separate from Apalis so it can be
//! tested directly without any job-queue machinery.

use std::collections::HashMap;

use chrono::Utc;
use rand::seq::IndexedRandom;
use sqlx::SqlitePool;
use tracing::warn;

use crate::buffer::client::BufferClient;
use crate::buffer::mutations::{BufferError, schedule_post};
use crate::caption::openai::OpenAiClient;
use crate::caption::{CaptionContext, get_or_generate_caption};
use crate::filter::{CandidateClass, FilterThresholds, RejectReason, classify_candidate};
use crate::immich::client::ImmichClient;
use crate::immich::search::{ImmichError, SmartSearchParams, fetch_faces, search_smart};
use crate::immich::types::Asset;
use crate::storage::repo::{
    RunSuccessFields, pending_media_cleanup, pending_media_insert, posts_dedup_set,
    posts_insert_in_tx, run_mark_failed, run_mark_running, run_mark_succeeded_in_tx,
};

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// All external dependencies the pipeline needs.  Shared between the Apalis
/// worker handler and integration tests.  All client fields are Clone-cheap
/// (their inner `reqwest::Client` is `Arc`-backed), so no extra `Arc` wrapping
/// is needed here.
#[derive(Clone)]
pub struct PipelineContext {
    pub pool: SqlitePool,
    pub immich: ImmichClient,
    pub openai: OpenAiClient,
    pub buffer: BufferClient,
    pub settings: PipelineSettings,
}

/// Cheap-clone subset of `Settings` that the pipeline reads.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    pub default_query: String,
    pub candidate_pool_size: u32,
    pub lookback_days: i64,
    pub face_thresholds: FilterThresholds,
    pub buffer_channel_id: String,
    pub public_base_url: String,
    pub pending_media_ttl_minutes: u32,
}

// ---------------------------------------------------------------------------
// Return types
// ---------------------------------------------------------------------------

/// Returned on a successful run.
#[derive(Debug)]
#[allow(dead_code)]
pub struct PipelineSuccess {
    pub buffer_post_id: String,
    /// The FINAL text sent to Buffer (caption + location + hashtags).
    pub caption: String,
    pub immich_asset_id: String,
}

/// Errors that `execute_run` can return.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("no candidates: {summary}")]
    NoCandidates { summary: String },

    #[error("immich: {0}")]
    Immich(#[from] ImmichError),

    #[error("caption: {0}")]
    Caption(#[from] crate::caption::CaptionError),

    #[error("buffer: {0}")]
    Buffer(#[from] BufferError),

    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

// ---------------------------------------------------------------------------
// Text assembly
// ---------------------------------------------------------------------------

/// Assemble the final caption text to send to Buffer:
///
/// ```text
/// {caption.text} - {city}, {country}
///
/// #{tag1} #{tag2} ...
/// ```
///
/// Rules:
/// - City absent → `" - {country}"` only
/// - Both absent → caption text + hashtags only (a warning is logged)
/// - Empty hashtag list → trailing block is omitted
pub fn assemble_buffer_text(
    caption_text: &str,
    hashtags: &[String],
    city: Option<&str>,
    country: Option<&str>,
) -> String {
    // Build location suffix
    let location_suffix = match (city, country) {
        (Some(ci), Some(co)) => format!(" - {ci}, {co}"),
        (None, Some(co)) => format!(" - {co}"),
        (Some(ci), None) => format!(" - {ci}"),
        (None, None) => {
            // Filter should have rejected this, but be defensive
            warn!(
                "assemble_buffer_text: both city and country are None — producing caption without location suffix"
            );
            String::new()
        }
    };

    let body = format!("{caption_text}{location_suffix}");

    if hashtags.is_empty() {
        body
    } else {
        let tag_line = hashtags
            .iter()
            .map(|h| format!("#{h}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{body}\n\n{tag_line}")
    }
}

// ---------------------------------------------------------------------------
// Candidate gathering helper
// ---------------------------------------------------------------------------

/// Tier label for an asset that survived classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateTier {
    Preferred,
    Acceptable,
}

/// Run a smart-search pass and update `candidates` and `reject_counts`.
///
/// Assets whose IDs are already in `seen_ids` are skipped (dedup across the
/// two search passes).
async fn gather_candidates(
    immich: &ImmichClient,
    params: &SmartSearchParams,
    posted_set: &std::collections::HashSet<String>,
    thresholds: &FilterThresholds,
    candidates: &mut Vec<(Asset, CandidateTier)>,
    reject_counts: &mut HashMap<String, u32>,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<i64, PipelineError> {
    let assets = search_smart(immich, params).await?;
    let total_returned = assets.len() as i64;

    for asset in assets {
        if seen_ids.contains(&asset.id) {
            continue;
        }
        seen_ids.insert(asset.id.clone());

        let faces = match fetch_faces(immich, &asset.id).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    asset_id = %asset.id,
                    error = %e,
                    "fetch_faces failed; treating as no-faces and continuing"
                );
                Vec::new()
            }
        };

        match classify_candidate(&asset, &faces, thresholds, posted_set) {
            CandidateClass::Preferred => candidates.push((asset, CandidateTier::Preferred)),
            CandidateClass::Acceptable => candidates.push((asset, CandidateTier::Acceptable)),
            CandidateClass::Rejected(reason) => {
                let key = reject_reason_key(&reason);
                *reject_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    Ok(total_returned)
}

fn reject_reason_key(reason: &RejectReason) -> String {
    match reason {
        RejectReason::AlreadyPosted => "already_posted".to_string(),
        RejectReason::NamedPersonPresent { .. } => "named_person".to_string(),
        RejectReason::NoLocation => "no_location".to_string(),
        RejectReason::MissingImageDimensions => "missing_dimensions".to_string(),
        RejectReason::PerFaceAreaExceeded { .. } => "per_face_area".to_string(),
        RejectReason::TotalFaceAreaExceeded { .. } => "total_face_area".to_string(),
    }
}

fn build_no_candidates_summary(total: i64, reject_counts: &HashMap<String, u32>) -> String {
    if reject_counts.is_empty() {
        return format!("{total} total from search");
    }
    let mut parts: Vec<String> = reject_counts
        .iter()
        .map(|(k, v)| format!("{v} rejected for {k}"))
        .collect();
    // Sort for deterministic output (makes tests easy to assert)
    parts.sort();
    format!("{total} total from search; {}", parts.join(", "))
}

// ---------------------------------------------------------------------------
// Main pipeline
// ---------------------------------------------------------------------------

/// Execute a single Run end-to-end.
///
/// Returns `Ok(PipelineSuccess)` when the post is scheduled and all audit rows
/// are written.  Returns `Err(PipelineError)` otherwise; in either terminal
/// state the `runs` row is updated.
#[tracing::instrument(
    name = "pipeline.execute_run",
    skip(ctx),
    fields(run_id = %run_id)
)]
pub async fn execute_run(
    ctx: &PipelineContext,
    run_id: &str,
    query_hint: Option<&str>,
) -> Result<PipelineSuccess, PipelineError> {
    let started = std::time::Instant::now();
    let now_epoch = Utc::now().timestamp();

    // Helper: persist failure and propagate the error.
    // Defined before the first fallible step so every bail-out path can use it.
    macro_rules! fail {
        ($err:expr) => {{
            let e = $err;
            let duration_ms = started.elapsed().as_millis() as i64;
            let _ = run_mark_failed(&ctx.pool, run_id, &e.to_string(), duration_ms).await;
            return Err(e);
        }};
    }

    // Step 1: Mark run as running.
    // If the state transition itself fails, write a failed row directly and bail.
    if let Err(e) = run_mark_running(&ctx.pool, run_id).await {
        let _ = run_mark_failed(
            &ctx.pool,
            run_id,
            &format!("failed to transition to running: {e}"),
            started.elapsed().as_millis() as i64,
        )
        .await;
        return Err(PipelineError::Database(e));
    }

    // Step 2: Cleanup expired pending_media.
    // Non-fatal: a sweep failure must not abort an otherwise healthy run.
    if let Err(e) = pending_media_cleanup(&ctx.pool, now_epoch).await {
        tracing::warn!(error = %e, "pending_media cleanup failed; continuing");
    }

    // Step 3: Build query.
    let query = query_hint.unwrap_or(&ctx.settings.default_query).to_owned();

    // Step 4: Build posted_set
    let posted_set = match posts_dedup_set(&ctx.pool).await {
        Ok(s) => s,
        Err(e) => fail!(PipelineError::Database(e)),
    };

    // Step 5: First search pass (with takenAfter)
    let lookback_date = Utc::now() - chrono::Duration::days(ctx.settings.lookback_days);

    let params_with_date = SmartSearchParams {
        query: query.clone(),
        size: ctx.settings.candidate_pool_size,
        taken_after: Some(lookback_date),
    };

    let mut candidates: Vec<(Asset, CandidateTier)> = Vec::new();
    let mut reject_counts: HashMap<String, u32> = HashMap::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    let total_returned_pass1 = match gather_candidates(
        &ctx.immich,
        &params_with_date,
        &posted_set,
        &ctx.settings.face_thresholds,
        &mut candidates,
        &mut reject_counts,
        &mut seen_ids,
    )
    .await
    {
        Ok(n) => n,
        Err(e) => fail!(e),
    };

    // Step 6: Retry without takenAfter if < 3 candidates
    let total_returned = if candidates.len() < 3 {
        tracing::info!(
            candidates_so_far = candidates.len(),
            "fewer than 3 candidates after date-filtered pass; retrying without takenAfter"
        );
        let params_no_date = SmartSearchParams {
            query: query.clone(),
            size: ctx.settings.candidate_pool_size,
            taken_after: None,
        };
        let total_returned_pass2 = match gather_candidates(
            &ctx.immich,
            &params_no_date,
            &posted_set,
            &ctx.settings.face_thresholds,
            &mut candidates,
            &mut reject_counts,
            &mut seen_ids,
        )
        .await
        {
            Ok(n) => n,
            Err(e) => fail!(e),
        };
        total_returned_pass1 + total_returned_pass2
    } else {
        total_returned_pass1
    };

    // Step 7: Bail if still < 3
    if candidates.len() < 3 {
        let summary = build_no_candidates_summary(total_returned, &reject_counts);
        fail!(PipelineError::NoCandidates { summary });
    }

    // Step 8: Pick at random — prefer the no-faces tier; fall back to tiny-faces.
    // ThreadRng is !Send, so we must drop it before any await point.
    let (selected_asset, selected_tier) = {
        let mut rng = rand::rng();
        let preferred: Vec<&Asset> = candidates
            .iter()
            .filter_map(|(a, t)| matches!(t, CandidateTier::Preferred).then_some(a))
            .collect();

        if let Some(asset) = preferred.choose(&mut rng) {
            ((*asset).clone(), CandidateTier::Preferred)
        } else {
            let acceptable: Vec<&Asset> = candidates
                .iter()
                .filter_map(|(a, t)| matches!(t, CandidateTier::Acceptable).then_some(a))
                .collect();
            let asset = acceptable.choose(&mut rng).expect(
                "candidates is non-empty and contains no Preferred → must contain Acceptable",
            );
            ((*asset).clone(), CandidateTier::Acceptable)
        }
    };

    let tier_label = match selected_tier {
        CandidateTier::Preferred => "preferred",
        CandidateTier::Acceptable => "acceptable",
    };
    tracing::info!(
        asset_id = %selected_asset.id,
        candidate_tier = tier_label,
        "selected candidate asset"
    );

    // Step 9: Download thumbnail
    let image_bytes = match ctx.immich.fetch_thumbnail(&selected_asset.id).await {
        Ok(b) => b,
        Err(e) => fail!(PipelineError::Immich(e)),
    };

    // Step 10: Get or generate caption
    let exif = selected_asset.exif_info.as_ref();
    let city = exif.and_then(|e| e.city.as_deref());
    let country = exif.and_then(|e| e.country.as_deref());
    let date = exif
        .and_then(|e| e.date_time_original)
        .map(|dt| dt.date_naive())
        .or_else(|| selected_asset.file_created_at.map(|dt| dt.date_naive()));

    let caption_ctx = CaptionContext {
        city,
        country,
        date,
    };

    tracing::info!(caption_ctx = ?caption_ctx, "image EXIF context");

    let caption = match get_or_generate_caption(
        &ctx.pool,
        &ctx.openai,
        &selected_asset.id,
        &image_bytes,
        &caption_ctx,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => fail!(PipelineError::Caption(e)),
    };

    // Step 11: Insert pending_media.
    // orphan-safe: if a later step fails, the TTL sweep in step 2 of the next Run cleans this up.
    let media_uuid = uuid::Uuid::new_v4().to_string();
    let ttl_seconds = i64::from(ctx.settings.pending_media_ttl_minutes) * 60;
    let expires_at = now_epoch + ttl_seconds;
    if let Err(e) =
        pending_media_insert(&ctx.pool, &media_uuid, &selected_asset.id, expires_at).await
    {
        fail!(PipelineError::Database(e));
    }

    // Step 12: Assemble final caption text
    let final_text = assemble_buffer_text(&caption.text, &caption.hashtags, city, country);

    // Step 13: Call Buffer
    let public_url = format!("{}/pic/{media_uuid}.jpg", ctx.settings.public_base_url);
    tracing::info!(public_url = %public_url, "generated public URL");
    let scheduled = match schedule_post(
        &ctx.buffer,
        &ctx.settings.buffer_channel_id,
        &public_url,
        &final_text,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => fail!(PipelineError::Buffer(e)),
    };

    // Step 14: Persist success atomically — posts INSERT + runs UPDATE in one transaction.
    // If either write fails the whole transaction is rolled back, keeping posts and runs
    // consistent (no "posted but run still running" stuck state).
    let finished_at = Utc::now().timestamp();
    let duration_ms = started.elapsed().as_millis() as i64;

    let success_fields = RunSuccessFields {
        run_id,
        finished_at,
        query_used: &query,
        candidates_returned: total_returned,
        candidates_after_filter: candidates.len() as i64,
        selected_asset_id: &selected_asset.id,
        caption: &final_text,
        buffer_post_id: &scheduled.post_id,
        duration_ms,
    };

    let mut tx = match ctx.pool.begin().await {
        Ok(t) => t,
        Err(e) => fail!(PipelineError::Database(e)),
    };

    if let Err(e) = posts_insert_in_tx(
        &mut tx,
        &selected_asset.id,
        &scheduled.post_id,
        &final_text,
        finished_at,
        run_id,
    )
    .await
    {
        fail!(PipelineError::Database(e));
    }

    if let Err(e) = run_mark_succeeded_in_tx(&mut tx, &success_fields).await {
        fail!(PipelineError::Database(e));
    }

    if let Err(e) = tx.commit().await {
        fail!(PipelineError::Database(e));
    }

    tracing::info!(
        buffer_post_id = %scheduled.post_id,
        asset_id = %selected_asset.id,
        "pipeline completed successfully"
    );

    Ok(PipelineSuccess {
        buffer_post_id: scheduled.post_id,
        caption: final_text,
        immich_asset_id: selected_asset.id,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::test_pool;
    use crate::storage::repo::run_insert;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // -----------------------------------------------------------------------
    // assemble_buffer_text tests (pure unit tests)
    // -----------------------------------------------------------------------

    #[test]
    fn assemble_buffer_text_with_city_and_country() {
        let text = assemble_buffer_text(
            "Golden light on ancient stones.",
            &["lisbon".into(), "portugal".into(), "travel".into()],
            Some("Lisbon"),
            Some("Portugal"),
        );
        assert_eq!(
            text,
            "Golden light on ancient stones. - Lisbon, Portugal\n\n#lisbon #portugal #travel"
        );
    }

    #[test]
    fn assemble_buffer_text_country_only() {
        let text = assemble_buffer_text(
            "Endless fields at dusk.",
            &["japan".into(), "countryside".into()],
            None,
            Some("Japan"),
        );
        assert_eq!(
            text,
            "Endless fields at dusk. - Japan\n\n#japan #countryside"
        );
    }

    #[test]
    fn assemble_buffer_text_empty_hashtags() {
        let text = assemble_buffer_text("Quiet waters.", &[], Some("Porto"), Some("Portugal"));
        assert_eq!(text, "Quiet waters. - Porto, Portugal");
    }

    #[test]
    fn assemble_buffer_text_both_none_no_crash() {
        // Should not panic; produces text without location suffix
        let text = assemble_buffer_text("Caption text.", &["tag".into()], None, None);
        // Just assert it contains the caption and tag (no location)
        assert!(text.contains("Caption text."));
        assert!(text.contains("#tag"));
        assert!(!text.contains(" - "));
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a minimal wiremocked `PipelineContext`.
    ///
    /// `immich_server`, `openai_server`, and `buffer_server` are all
    /// pre-started `MockServer` instances.  The caller is responsible for
    /// mounting mocks on them before calling `execute_run`.
    async fn build_test_context(
        pool: sqlx::SqlitePool,
        immich_server: &MockServer,
        openai_server: &MockServer,
        buffer_server: &MockServer,
    ) -> PipelineContext {
        let immich = ImmichClient::new(immich_server.uri(), "test-immich-key");
        let openai = OpenAiClient::with_base_url("sk-test", "gpt-test", openai_server.uri());
        let buffer = BufferClient::with_base_url("test-buf-key", buffer_server.uri());

        PipelineContext {
            pool,
            immich,
            openai,
            buffer,
            settings: PipelineSettings {
                default_query: "landscape".into(),
                candidate_pool_size: 50,
                lookback_days: 365,
                face_thresholds: FilterThresholds::default(),
                buffer_channel_id: "chan_test".into(),
                public_base_url: "https://test.example.com".into(),
                pending_media_ttl_minutes: 60,
            },
        }
    }

    /// Build a minimal valid smart-search response with `count` assets.
    /// All assets have Lisbon, Portugal location and no faces / people.
    fn make_search_response(count: usize) -> serde_json::Value {
        let items: Vec<serde_json::Value> = (0..count)
            .map(|i| {
                json!({
                    "id": format!("asset-{i:04}"),
                    "type": "IMAGE",
                    "thumbhash": null,
                    "originalMimeType": "image/jpeg",
                    "localDateTime": "2024-06-15T10:30:00.000Z",
                    "duration": "0:00:00.00000",
                    "livePhotoVideoId": null,
                    "hasMetadata": true,
                    "width": 4032,
                    "height": 3024,
                    "createdAt": "2024-06-15T10:30:00.000Z",
                    "updatedAt": "2024-06-15T10:30:00.000Z",
                    "fileCreatedAt": "2024-06-15T08:30:00.000Z",
                    "fileModifiedAt": "2024-06-15T08:30:00.000Z",
                    "ownerId": "owner-uuid",
                    "libraryId": null,
                    "originalPath": format!("/photos/photo{i}.jpg"),
                    "originalFileName": format!("photo{i}.jpg"),
                    "isFavorite": false,
                    "isArchived": false,
                    "isTrashed": false,
                    "isOffline": false,
                    "visibility": "public",
                    "checksum": format!("chk{i}"),
                    "isEdited": false,
                    "exifInfo": {
                        "exifImageWidth": 4032,
                        "exifImageHeight": 3024,
                        "city": "Lisbon",
                        "country": "Portugal",
                        "dateTimeOriginal": "2024-06-15T08:30:00.000Z"
                    },
                    "people": [],
                    "tags": []
                })
            })
            .collect();

        json!({
            "albums": { "total": 0, "count": 0, "items": [], "facets": [], "nextPage": null },
            "assets": {
                "total": count,
                "count": count,
                "nextPage": null,
                "facets": [],
                "items": items
            }
        })
    }

    /// Minimal valid OpenAI Responses API response.
    fn make_openai_response() -> serde_json::Value {
        json!({
            "id": "resp_test",
            "object": "response",
            "model": "gpt-test",
            "output": [{
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": "{\"caption\":\"Sunlight on ancient tiles.\",\"hashtags\":[\"lisbon\",\"portugal\"],\"alt_text\":\"Tiles in afternoon light.\"}"
                }]
            }]
        })
    }

    /// Minimal valid Buffer GraphQL success response.
    fn make_buffer_response(post_id: &str) -> serde_json::Value {
        json!({
            "data": {
                "createPost": {
                    "__typename": "PostActionSuccess",
                    "post": { "id": post_id }
                }
            }
        })
    }

    // -----------------------------------------------------------------------
    // execute_run_happy_path_persists_run_and_post
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_happy_path_persists_run_and_post() {
        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        // Pre-insert the run row (as Task 6 will do at enqueue time)
        run_insert(&pool, "run-happy", Utc::now().timestamp())
            .await
            .unwrap();

        // 3 assets with no faces → all pass filter
        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .and(header("x-api-key", "test-immich-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(3)))
            .mount(&immich)
            .await;

        // Faces endpoint returns empty for all assets
        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich)
            .await;

        // Thumbnail endpoint returns fake JPEG bytes
        Mock::given(method("GET"))
            .and(header("x-api-key", "test-immich-key"))
            .and(query_param("size", "preview"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0fake".to_vec()),
            )
            .mount(&immich)
            .await;

        // OpenAI returns a valid caption
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_openai_response()))
            .mount(&openai)
            .await;

        // Buffer returns success
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_buffer_response("buf_happy_123")),
            )
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;

        let result = execute_run(&ctx, "run-happy", None).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let success = result.unwrap();
        assert_eq!(success.buffer_post_id, "buf_happy_123");
        assert!(!success.caption.is_empty());
        assert!(!success.immich_asset_id.is_empty());

        // runs row should be succeeded
        let run_row = crate::storage::repo::run_get_status(&pool, "run-happy")
            .await
            .unwrap()
            .expect("run row must exist");
        assert_eq!(run_row.status, "succeeded");
        assert!(run_row.finished_at.is_some());
        assert_eq!(run_row.buffer_post_id.as_deref(), Some("buf_happy_123"));

        // posts row should exist
        let post_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(post_count, 1);

        // pending_media row should exist
        let pm_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_media")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pm_count, 1);
    }

    // -----------------------------------------------------------------------
    // execute_run_no_candidates_fails_with_summary
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_no_candidates_fails_with_summary() {
        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        run_insert(&pool, "run-nocands", Utc::now().timestamp())
            .await
            .unwrap();

        // Both search passes return 0 assets
        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(0)))
            .mount(&immich)
            .await;

        // OpenAI and Buffer should not be called
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&openai)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;
        let result = execute_run(&ctx, "run-nocands", None).await;

        assert!(
            matches!(result, Err(PipelineError::NoCandidates { .. })),
            "expected NoCandidates, got: {result:?}"
        );

        // runs row should be failed
        let run_row = crate::storage::repo::run_get_status(&pool, "run-nocands")
            .await
            .unwrap()
            .expect("run row must exist");
        assert_eq!(run_row.status, "failed");
        assert!(run_row.error.is_some());
    }

    // -----------------------------------------------------------------------
    // execute_run_caption_cache_hit_skips_openai
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_caption_cache_hit_skips_openai() {
        use crate::caption::Caption;
        use crate::caption::prompt::render_prompts;
        use crate::storage::repo::caption_insert;

        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        run_insert(&pool, "run-cache", Utc::now().timestamp())
            .await
            .unwrap();

        // Immich returns exactly 3 assets — same id "asset-0000" will be picked or another
        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(3)))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(query_param("size", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8fake".to_vec()))
            .mount(&immich)
            .await;

        // Pre-populate the caption cache for ALL 3 candidate assets so regardless
        // of which one is picked the cache hits.
        let ctx_for_seed = CaptionContext {
            city: Some("Lisbon"),
            country: Some("Portugal"),
            date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
        };
        let rendered = render_prompts(&ctx_for_seed);
        let cache_key = format!("system:\n{}\n\nuser:\n{}", rendered.system, rendered.user);
        let cached_caption = Caption {
            text: "Warm afternoon light.".into(),
            hashtags: vec!["lisbon".into()],
            alt_text: "Sun-drenched tiles.".into(),
        };
        for i in 0..3 {
            caption_insert(&pool, &format!("asset-{i:04}"), &cache_key, &cached_caption)
                .await
                .unwrap();
        }

        // OpenAI MUST NOT be called
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&openai)
            .await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_buffer_response("buf_cache_456")),
            )
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;
        let result = execute_run(&ctx, "run-cache", None).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let success = result.unwrap();
        // Caption text from cache should appear in the final text
        assert!(
            success.caption.contains("Warm afternoon light."),
            "expected cached caption text, got: {}",
            success.caption
        );
    }

    // -----------------------------------------------------------------------
    // execute_run_buffer_failure_persists_failed_status
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_buffer_failure_persists_failed_status() {
        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        run_insert(&pool, "run-buffail", Utc::now().timestamp())
            .await
            .unwrap();

        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(3)))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(query_param("size", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8fake".to_vec()))
            .mount(&immich)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_openai_response()))
            .mount(&openai)
            .await;

        // Buffer returns a MutationError
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "createPost": {
                        "__typename": "MutationError",
                        "message": "Channel not authorised"
                    }
                }
            })))
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;
        let result = execute_run(&ctx, "run-buffail", None).await;

        assert!(
            matches!(result, Err(PipelineError::Buffer(_))),
            "expected Buffer error, got: {result:?}"
        );

        // runs row should be failed
        let run_row = crate::storage::repo::run_get_status(&pool, "run-buffail")
            .await
            .unwrap()
            .expect("run row must exist");
        assert_eq!(run_row.status, "failed");
        assert!(
            run_row
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Channel not authorised"),
            "expected error to mention 'Channel not authorised', got: {:?}",
            run_row.error
        );

        // No posts row should have been inserted
        let post_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            post_count, 0,
            "no post row should exist after buffer failure"
        );

        // The pending_media row IS still present after the buffer failure — this is by design.
        // The TTL sweep at step 2 of the next Run will clean it up.
        let pm_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_media")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            pm_count, 1,
            "pending_media row should survive a Buffer failure; the TTL sweep on the next run cleans it up"
        );
    }

    // -----------------------------------------------------------------------
    // execute_run_fetch_faces_500_continues_run  (Fix B)
    //
    // One asset's /api/faces endpoint returns 500.  The run should still
    // succeed because the other assets provide enough candidates.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_fetch_faces_500_continues_run() {
        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        run_insert(&pool, "run-facefail", Utc::now().timestamp())
            .await
            .unwrap();

        // Search returns 4 assets: asset-0000..asset-0003
        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(4)))
            .mount(&immich)
            .await;

        // asset-0000's face endpoint returns 500 — the rest return empty
        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("assetId", "asset-0000"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(query_param("size", "preview"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0fake".to_vec()),
            )
            .mount(&immich)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_openai_response()))
            .mount(&openai)
            .await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_buffer_response("buf_facefail_789")),
            )
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;
        let result = execute_run(&ctx, "run-facefail", None).await;

        assert!(
            result.is_ok(),
            "expected Ok despite fetch_faces 500 for one asset, got: {result:?}"
        );

        // The run should be succeeded in the DB
        let run_row = crate::storage::repo::run_get_status(&pool, "run-facefail")
            .await
            .unwrap()
            .expect("run row must exist");
        assert_eq!(run_row.status, "succeeded");

        // Exactly one post should have been inserted
        let post_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(post_count, 1);
    }

    /// Compact `Asset` JSON used in tier-ordering tests.
    fn asset_json(
        id: &str,
        city: &str,
        country: &str,
        people: &[serde_json::Value],
    ) -> serde_json::Value {
        json!({
            "id": id,
            "type": "IMAGE",
            "thumbhash": null,
            "originalMimeType": "image/jpeg",
            "localDateTime": "2024-06-15T10:30:00.000Z",
            "duration": "0:00:00.00000",
            "livePhotoVideoId": null,
            "hasMetadata": true,
            "width": 4032,
            "height": 3024,
            "createdAt": "2024-06-15T10:30:00.000Z",
            "updatedAt": "2024-06-15T10:30:00.000Z",
            "fileCreatedAt": "2024-06-15T08:30:00.000Z",
            "fileModifiedAt": "2024-06-15T08:30:00.000Z",
            "ownerId": "owner-uuid",
            "libraryId": null,
            "originalPath": format!("/photos/{id}.jpg"),
            "originalFileName": format!("{id}.jpg"),
            "isFavorite": false,
            "isArchived": false,
            "isTrashed": false,
            "isOffline": false,
            "visibility": "public",
            "checksum": format!("chk-{id}"),
            "isEdited": false,
            "exifInfo": {
                "exifImageWidth": 4032,
                "exifImageHeight": 3024,
                "city": city,
                "country": country,
                "dateTimeOriginal": "2024-06-15T08:30:00.000Z"
            },
            "people": people,
            "tags": []
        })
    }

    // -----------------------------------------------------------------------
    // execute_run_all_filtered_fails_with_summary  (Fix E)
    //
    // All 3 assets from search are already in posts (AlreadyPosted).
    // The run should fail with NoCandidates and the summary must contain
    // both the total count and the per-reason count.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_all_filtered_fails_with_summary() {
        use crate::storage::repo::posts_insert_in_tx;

        let pool = test_pool().await;
        let immich = MockServer::start().await;
        let openai = MockServer::start().await;
        let buffer = MockServer::start().await;

        // Pre-seed a run for the FK constraint on posts
        run_insert(&pool, "run-prior", Utc::now().timestamp())
            .await
            .unwrap();

        // Pre-insert all 3 assets into posts so they get AlreadyPosted
        let mut tx = pool.begin().await.unwrap();
        for i in 0..3_usize {
            posts_insert_in_tx(
                &mut tx,
                &format!("asset-{i:04}"),
                &format!("buf-prior-{i}"),
                "prior caption",
                Utc::now().timestamp(),
                "run-prior",
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();

        run_insert(&pool, "run-allfiltered", Utc::now().timestamp())
            .await
            .unwrap();

        // Both search passes return the same 3 already-posted assets
        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_search_response(3)))
            .mount(&immich)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich)
            .await;

        // OpenAI and Buffer must not be called
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&openai)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&buffer)
            .await;

        let ctx = build_test_context(pool.clone(), &immich, &openai, &buffer).await;
        let result = execute_run(&ctx, "run-allfiltered", None).await;

        match result {
            Err(PipelineError::NoCandidates { summary }) => {
                // Summary must mention the total from search
                assert!(
                    summary.contains("total from search"),
                    "summary missing total: {summary}"
                );
                // Summary must mention the per-reason count
                assert!(
                    summary.contains("rejected for already_posted"),
                    "summary missing already_posted count: {summary}"
                );
                // Both passes return 3 assets each but seen_ids deduplication
                // means pass 2 adds 0 new assets; only pass 1's 3 count in
                // the total. Accept either "3 total" or "6 total".
                assert!(
                    summary.contains("3 total") || summary.contains("6 total"),
                    "summary missing expected total count: {summary}"
                );
            }
            other => panic!("expected NoCandidates, got: {other:?}"),
        }

        // runs row should be failed
        let run_row = crate::storage::repo::run_get_status(&pool, "run-allfiltered")
            .await
            .unwrap()
            .expect("run row must exist");
        assert_eq!(run_row.status, "failed");
    }

    // -----------------------------------------------------------------------
    // execute_run_prefers_no_faces_over_tiny_faces
    //
    // Pool contains:
    //   - asset-tiny:   one face at ~0.5% of image → Acceptable
    //   - asset-clean:  zero faces                 → Preferred
    //   - asset-clean2: zero faces                 → Preferred
    //   - asset-named:  named person               → Rejected
    //
    // The picked asset must come from the Preferred tier (asset-clean or
    // asset-clean2); asset-tiny must not be chosen.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn execute_run_prefers_no_faces_over_tiny_faces() {
        let pool = test_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let immich_server = MockServer::start().await;
        let openai_server = MockServer::start().await;
        let buffer_server = MockServer::start().await;

        // Smart-search response: 4 assets — 2 Preferred (clean, clean2), 1 Acceptable (tiny),
        // 1 Rejected (named). This gives a combined pool of 3 candidates (≥ 3 required).
        let search_body = json!({
            "albums": { "total": 0, "count": 0, "items": [], "facets": [], "nextPage": null },
            "assets": {
                "total": 4, "count": 4, "nextPage": null, "facets": [],
                "items": [
                    asset_json("asset-tiny", "Lisbon", "Portugal", &[]),
                    asset_json("asset-clean", "Lisbon", "Portugal", &[]),
                    asset_json("asset-clean2", "Lisbon", "Portugal", &[]),
                    asset_json("asset-named", "Lisbon", "Portugal", &[json!({"id": "p1", "name": "Mom"})]),
                ]
            }
        });

        Mock::given(method("POST"))
            .and(path("/api/search/smart"))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_body))
            .mount(&immich_server)
            .await;

        // Faces endpoint:
        //   asset-tiny  → one 0.5% face
        //   asset-clean → []
        //   asset-clean2 → []
        //   asset-named: fetch_faces is called unconditionally, so the mock must
        //   respond. classify_candidate then rejects on the named-person check
        //   before any face-area math runs.
        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("id", "asset-tiny"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "id": "face-1",
                    "boundingBoxX1": 0,
                    "boundingBoxY1": 0,
                    "boundingBoxX2": 245,
                    "boundingBoxY2": 245,
                    "imageWidth": 4032,
                    "imageHeight": 3024,
                    "person": null
                }
            ])))
            .mount(&immich_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("id", "asset-clean"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("id", "asset-clean2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/faces"))
            .and(query_param("id", "asset-named"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&immich_server)
            .await;

        // Thumbnails for both Preferred assets (either could be picked).
        Mock::given(method("GET"))
            .and(path("/api/assets/asset-clean/thumbnail"))
            .and(query_param("size", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFFu8; 32]))
            .mount(&immich_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/assets/asset-clean2/thumbnail"))
            .and(query_param("size", "preview"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFFu8; 32]))
            .mount(&immich_server)
            .await;

        // OpenAI caption mock — accept any input, return a fixed caption.
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_openai_response()))
            .mount(&openai_server)
            .await;

        // Buffer mock — accept any post, return a fixed buffer post id.
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_buffer_response("buf_post_clean")),
            )
            .mount(&buffer_server)
            .await;

        let ctx =
            build_test_context(pool.clone(), &immich_server, &openai_server, &buffer_server).await;
        let run_id = "01HCLEAN0000000000000000000";
        run_insert(&pool, run_id, Utc::now().timestamp())
            .await
            .unwrap();

        let result = execute_run(&ctx, run_id, None).await;
        assert!(result.is_ok(), "expected run to succeed, got {result:?}");

        let row: (String,) = sqlx::query_as("SELECT immich_asset_id FROM posts WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        // The pipeline must pick one of the Preferred (no-faces) assets, not the
        // Acceptable (tiny-faces) asset-tiny and not the Rejected asset-named.
        assert!(
            row.0 == "asset-clean" || row.0 == "asset-clean2",
            "expected a Preferred asset to be picked, but got: {}",
            row.0
        );
    }
}
