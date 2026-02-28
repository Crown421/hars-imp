/// Application name, read from Cargo.toml at compile time.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Application version, read from Cargo.toml at compile time.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Repository URL, read from Cargo.toml at compile time.
pub const APP_REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
