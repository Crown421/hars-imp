use thiserror::Error;

/// Top-level application error type.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("MQTT error: {0}")]
    Mqtt(#[from] Box<MqttError>),

    #[error("D-Bus error: {0}")]
    Dbus(#[from] DbusError),

    #[error("Component error: {0}")]
    Component(#[from] ComponentError),
}

/// Errors related to configuration loading and validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("Invalid configuration: {0}")]
    Validation(String),
}

/// Errors related to MQTT operations.
#[derive(Debug, Error)]
pub enum MqttError {
    #[error("MQTT client error: {0}")]
    Client(#[from] rumqttc::ClientError),

    #[error("MQTT connection error: {0}")]
    Connection(#[from] rumqttc::ConnectionError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("TLS configuration error: {0}")]
    Tls(String),
}

/// Errors related to D-Bus operations.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum DbusError {
    #[error("D-Bus error: {0}")]
    Zbus(#[from] zbus::Error),

    #[error("D-Bus connection failed after retries")]
    ConnectionFailed,

    #[error("D-Bus inhibitor state error: {0}")]
    InhibitorState(String),
}

/// Errors related to component operations.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ComponentError {
    #[error("Command execution failed: {0}")]
    CommandFailed(String),

    #[error("Invalid payload: {0}")]
    InvalidPayload(String),

    #[error("Discovery conflict: {0}")]
    DiscoveryConflict(String),
}
