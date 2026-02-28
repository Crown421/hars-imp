use tracing_subscriber::{EnvFilter, fmt};

/// Initialize the tracing subscriber with the given log level.
///
/// Respects `RUST_LOG` env var if set, otherwise uses the config level.
pub fn init(log_level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .init();
}
