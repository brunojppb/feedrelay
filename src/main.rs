use actix_web::{App, HttpServer, web};
use anyhow::Context;

mod auth;
mod buffer;
mod caption;
mod config;
mod error;
mod filter;
mod immich;
mod jobs;
mod pipeline;
mod routes;
mod storage;
mod telemetry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present (silently ignored if missing)
    dotenvy::dotenv().ok();

    // Load settings from environment variables
    let settings =
        config::Settings::from_env().context("failed to load configuration from environment")?;

    // Initialise JSON tracing to stdout
    telemetry::init_tracing(&settings.rust_log);

    tracing::info!(
        port = settings.port,
        database_url = %settings.database_url,
        "feedrelay starting"
    );

    if settings.shortcut_token.is_none() {
        tracing::warn!(
            "SHORTCUT_TOKEN is not set; all /trigger/* requests will be rejected with 401"
        );
    }

    // Open the SQLite pool
    let pool = storage::db::build_pool(&settings.database_url)
        .await
        .context("failed to open SQLite pool")?;

    // Run Apalis migrations first (their schema is independent from ours)
    jobs::setup_storage(&pool)
        .await
        .context("failed to run Apalis SQLite migrations")?;

    tracing::info!("apalis migrations applied");

    // Run our own app migrations
    storage::db::run_migrations(&pool)
        .await
        .context("failed to run database migrations")?;

    tracing::info!("database migrations applied");

    // Build PipelineContext from settings.
    // We require Immich, OpenAI, and Buffer credentials to start the worker.
    // The server binds first; missing credentials cause a panic here so the
    // process exits with a clear message before accepting traffic.
    let immich_base_url = settings
        .immich_base_url
        .clone()
        .expect("IMMICH_BASE_URL is required to run the worker");
    let immich_api_key = settings
        .immich_api_key
        .clone()
        .expect("IMMICH_API_KEY is required to run the worker");
    let openai_api_key = settings
        .openai_api_key
        .clone()
        .expect("OPENAI_API_KEY is required to run the worker");
    let buffer_api_key = settings
        .buffer_api_key
        .clone()
        .expect("BUFFER_API_KEY is required to run the worker");
    let buffer_channel_id = settings
        .buffer_instagram_channel_id
        .clone()
        .expect("BUFFER_INSTAGRAM_CHANNEL_ID is required to run the worker");

    let pipeline_ctx = pipeline::PipelineContext {
        pool: pool.clone(),
        immich: immich::client::ImmichClient::new(immich_base_url, immich_api_key),
        openai: caption::OpenAiClient::new(openai_api_key, settings.openai_model.clone()),
        buffer: buffer::client::BufferClient::with_base_url(
            buffer_api_key,
            settings.buffer_graphql_url.clone(),
        ),
        settings: pipeline::PipelineSettings {
            default_query: settings.immich_default_query.clone(),
            candidate_pool_size: settings.immich_candidate_pool_size,
            lookback_days: settings.immich_lookback_days,
            face_thresholds: filter::FilterThresholds {
                per_face_pct: settings.face_area_per_face_pct,
                total_pct: settings.face_area_total_pct,
            },
            buffer_channel_id,
            public_base_url: settings.public_base_url.trim_end_matches('/').to_string(),
            pending_media_ttl_minutes: settings.pending_media_ttl_minutes,
        },
    };

    let pool_data = web::Data::new(pool.clone());
    let immich_data = web::Data::new(pipeline_ctx.immich.clone());
    let port = settings.port;
    let shortcut_token = settings.shortcut_token.clone();

    let server = HttpServer::new(move || {
        let auth_cfg = web::Data::new(auth::AuthConfig {
            expected_token: shortcut_token.clone(),
        });

        App::new()
            .wrap(tracing_actix_web::TracingLogger::default())
            .app_data(auth_cfg)
            .app_data(pool_data.clone())
            .app_data(immich_data.clone())
            .route("/management/health", web::get().to(routes::health::health))
            .route("/pic/{uuid}.jpg", web::get().to(routes::pic::serve_pic))
            .service(
                web::scope("/trigger")
                    .wrap(actix_web::middleware::from_fn(auth::bearer_auth_middleware))
                    .route("/post", web::post().to(routes::trigger::post))
                    .route("/status/{run_id}", web::get().to(routes::trigger::status)),
            )
    })
    .bind(("0.0.0.0", port))
    .with_context(|| format!("failed to bind to port {port}"))?
    .run();

    tracing::info!(port = port, "server listening");

    // Spawn the Apalis worker as a tokio task alongside the actix server.
    // If the worker exits unexpectedly, we log the error but let the server
    // keep running (admin can restart the process).
    let worker_pool = pool.clone();
    let worker_handle = tokio::spawn(async move {
        tracing::info!("apalis worker starting");
        if let Err(e) = jobs::run_worker(&worker_pool, pipeline_ctx).await {
            tracing::error!(error = %e, "apalis worker exited with error");
        }
    });

    // Wait for the actix server to finish (driven by SIGINT/SIGTERM).
    // The worker runs in the background; it will be dropped when the process exits.
    let server_result = server.await;

    // Worker is aborted abruptly: any in-flight pipeline future is cancelled at
    // its next await point. The corresponding `runs` row may stay in 'running'
    // after a forced shutdown. A startup-time stuck-run cleanup belongs in
    // Task 7 (deployment hardening).
    worker_handle.abort();

    server_result.context("server error")?;

    tracing::info!("server stopped");

    Ok(())
}
