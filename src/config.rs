use config::{Config, ConfigError};
use serde::Deserialize;

/// Application settings loaded from environment variables.
///
/// ## Immich fields: `Option<String>` vs required `String`
///
/// `immich_base_url` and `immich_api_key` are typed as `Option<String>`.
/// Rationale: making them required `String` would break `cargo run` for anyone
/// who only wants the health endpoint (e.g. local development, CI smoke tests).
/// Task 5 will unwrap / validate them when the worker is wired up — at that
/// point the worker can fail fast with a clear error if either is missing.
/// All other Immich fields carry `serde` defaults that match the PRD §8 table.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    /// TCP port to bind the HTTP server to. Defaults to 8080.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Log filter directives (RUST_LOG format). Defaults to `feedrelay=debug,info`.
    #[serde(default = "default_rust_log")]
    pub rust_log: String,

    /// SQLite connection URL, e.g. `sqlite:///data/feedrelay.db` (relative paths work too).
    /// Required — the server will not start without it.
    pub database_url: String,

    // -------------------------------------------------------------------------
    // Immich
    // -------------------------------------------------------------------------
    /// Base URL of the Immich instance, e.g. `"https://immich.example.com"`.
    /// Optional here; Task 5's worker will require it at startup.
    pub immich_base_url: Option<String>,

    /// Immich API key (from admin UI → API Keys).
    /// Optional here; Task 5's worker will require it at startup.
    pub immich_api_key: Option<String>,

    /// CLIP natural-language query for smart search.
    /// Env: `IMMICH_DEFAULT_QUERY`.  Default: `"landscape architecture nature scenery"`.
    #[serde(default = "default_immich_query")]
    pub immich_default_query: String,

    /// Maximum assets to request per smart-search call.
    /// Env: `IMMICH_CANDIDATE_POOL_SIZE`.  Default: `50`.
    #[serde(default = "default_candidate_pool_size")]
    pub immich_candidate_pool_size: u32,

    /// How many days back the `takenAfter` window extends.
    /// Env: `IMMICH_LOOKBACK_DAYS`.  Default: `365`.
    #[serde(default = "default_lookback_days")]
    pub immich_lookback_days: i64,

    // -------------------------------------------------------------------------
    // Face-area filter thresholds
    // -------------------------------------------------------------------------
    /// Maximum area a single face bounding box may occupy, as a percentage of
    /// total image area.  Env: `FACE_AREA_PER_FACE_PCT`.  Default: `1.0`.
    #[serde(default = "default_face_area_per_face_pct")]
    pub face_area_per_face_pct: f64,

    /// Maximum *combined* area of all face bounding boxes, as a percentage.
    /// Env: `FACE_AREA_TOTAL_PCT`.  Default: `2.0`.
    #[serde(default = "default_face_area_total_pct")]
    pub face_area_total_pct: f64,

    // -------------------------------------------------------------------------
    // OpenAI
    // -------------------------------------------------------------------------
    /// OpenAI API key (from platform.openai.com → API Keys).
    /// Optional here; Task 5's worker will unwrap / validate it at startup.
    /// Env: `OPENAI_API_KEY`.
    pub openai_api_key: Option<String>,

    /// OpenAI model to use for the Responses API.
    /// Env: `OPENAI_MODEL`.  Default: `"gpt-5.4-mini"` (per PRD §7 / §8).
    #[serde(default = "default_openai_model")]
    pub openai_model: String,

    // -------------------------------------------------------------------------
    // Buffer
    // -------------------------------------------------------------------------
    /// Buffer API key (from buffer.com → Account → API Access).
    /// Optional here; Task 5's worker will require it at startup.
    /// Env: `BUFFER_API_KEY`.
    pub buffer_api_key: Option<String>,

    /// Buffer GraphQL endpoint URL.
    /// Env: `BUFFER_GRAPHQL_URL`.  Default: `"https://api.buffer.com"`.
    /// Note: the endpoint does NOT have a `/graphql` path suffix — POST to the
    /// root URL directly (verified against Buffer API docs 2026-05-24).
    #[serde(default = "default_buffer_graphql_url")]
    pub buffer_graphql_url: String,

    /// Buffer Instagram channel ID to post to.
    /// Optional here; Task 5's worker will require it at startup.
    /// Env: `BUFFER_INSTAGRAM_CHANNEL_ID`.
    pub buffer_instagram_channel_id: Option<String>,

    // -------------------------------------------------------------------------
    // Auth
    // -------------------------------------------------------------------------
    /// Bearer token for the `/trigger/*` endpoints.
    ///
    /// If `None` (env var `SHORTCUT_TOKEN` not set), a warning is emitted at
    /// startup and all `/trigger/*` requests are rejected with 401.
    /// Env: `SHORTCUT_TOKEN`.
    pub shortcut_token: Option<String>,

    // -------------------------------------------------------------------------
    // Public URL / pending media
    // -------------------------------------------------------------------------
    /// Public base URL of this service, used to build `/pic/<uuid>.jpg` URLs that
    /// Buffer will fetch when scheduling a post.
    /// Env: `PUBLIC_BASE_URL`.  **Required** — no default.
    pub public_base_url: String,

    /// How long a `pending_media` row lives before it is eligible for cleanup (minutes).
    /// Env: `PENDING_MEDIA_TTL_MINUTES`.  Default: `60`.
    #[serde(default = "default_pending_media_ttl_minutes")]
    pub pending_media_ttl_minutes: u32,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

fn default_port() -> u16 {
    8080
}

fn default_rust_log() -> String {
    "feedrelay=debug,info".to_string()
}

fn default_immich_query() -> String {
    "landscape architecture nature scenery".to_string()
}

fn default_candidate_pool_size() -> u32 {
    50
}

fn default_lookback_days() -> i64 {
    365
}

fn default_face_area_per_face_pct() -> f64 {
    1.0
}

fn default_face_area_total_pct() -> f64 {
    2.0
}

fn default_openai_model() -> String {
    "gpt-5.4-mini".to_string()
}

fn default_buffer_graphql_url() -> String {
    "https://api.buffer.com".to_string()
}

fn default_pending_media_ttl_minutes() -> u32 {
    60
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

impl Settings {
    /// Load settings from environment variables (and an optional `.env` file).
    /// `dotenvy::dotenv()` must be called before this.
    pub fn from_env() -> Result<Self, ConfigError> {
        Config::builder()
            .add_source(
                config::Environment::default()
                    // Map e.g. `RUST_LOG` → `rust_log`, `DATABASE_URL` → `database_url`
                    .prefix_separator("_")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?
            .try_deserialize()
    }
}
