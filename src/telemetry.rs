use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialise the tracing subscriber with a JSON formatter writing to stdout.
/// The log filter is controlled by the `RUST_LOG` environment variable;
/// `default_filter` is used as a fallback when `RUST_LOG` is not set.
pub fn init_tracing(default_filter: &str) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().json().with_target(true))
        .try_init();
}
