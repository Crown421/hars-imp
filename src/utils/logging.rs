use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init_tracing(log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_new(log_level).or_else(|_| EnvFilter::try_new("info"))?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    Ok(())
}
