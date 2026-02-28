mod components;
mod config;
mod dbus;
mod error;
mod mqtt;
mod orchestrator;
mod util;

use tracing::{error, info};

#[tokio::main]
async fn main() {
    // Load config first (before logging, since log level comes from config).
    let config = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    // Initialize logging.
    util::logging::init(&config.log_level);

    info!(
        "{} v{} starting",
        util::version::APP_NAME,
        util::version::APP_VERSION
    );
    info!("Hostname: {}", config.hostname);

    // Run the orchestrator.
    let orchestrator = orchestrator::Orchestrator::new(config);
    if let Err(e) = orchestrator.run().await {
        error!("Application error: {e}");
        std::process::exit(1);
    }
}
