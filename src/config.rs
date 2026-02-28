use serde::Deserialize;
use std::path::PathBuf;
use tracing::info;

use crate::error::ConfigError;

/// Top-level application configuration, loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Hostname used as the device identifier in HA.
    pub hostname: String,

    /// MQTT broker address.
    pub mqtt_url: String,

    /// MQTT broker port.
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,

    /// MQTT username.
    pub username: String,

    /// MQTT password.
    pub password: String,

    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Interval in seconds for polled sensors.
    #[serde(default = "default_update_interval")]
    pub update_interval_secs: u64,

    /// Button definitions.
    #[serde(default)]
    pub button: Vec<ButtonConfig>,

    /// Switch definitions.
    #[serde(default)]
    pub switch: Vec<SwitchConfig>,
}

/// Configuration for a button entity.
#[derive(Debug, Clone, Deserialize)]
pub struct ButtonConfig {
    /// Display name (also forms the topic suffix).
    pub name: String,

    /// Shell command to execute on press.
    pub exec: String,
}

/// Configuration for a switch entity.
#[derive(Debug, Clone, Deserialize)]
pub struct SwitchConfig {
    /// Display name.
    pub name: String,

    /// Shell command to execute (mutually exclusive with `dbus`).
    pub exec: Option<String>,

    /// D-Bus method call spec (mutually exclusive with `exec`).
    pub dbus: Option<DbusActionConfig>,
}

/// D-Bus method call specification for a switch.
#[derive(Debug, Clone, Deserialize)]
pub struct DbusActionConfig {
    /// D-Bus service name (e.g. `org.example.Service`).
    pub service: String,

    /// D-Bus object path (e.g. `/org/example`).
    pub path: String,

    /// D-Bus interface name.
    pub interface: String,

    /// D-Bus method name.
    pub method: String,
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_update_interval() -> u64 {
    60
}

impl Config {
    /// Load configuration from the standard file path.
    ///
    /// - Debug builds: `./config.toml`
    /// - Release builds: `$HOME/.config/hars-imp/config.toml`
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        info!("Loading config from: {}", path.display());
        let contents = std::fs::read_to_string(&path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from a specific path.
    #[allow(dead_code)]
    pub fn load_from(path: &std::path::Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn config_path() -> Result<PathBuf, ConfigError> {
        if cfg!(debug_assertions) {
            Ok(PathBuf::from("./config.toml"))
        } else {
            let home = std::env::var("HOME")
                .map_err(|_| ConfigError::Validation("HOME environment variable not set".into()))?;
            Ok(PathBuf::from(home).join(".config/hars-imp/config.toml"))
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.hostname.is_empty() {
            return Err(ConfigError::Validation("hostname cannot be empty".into()));
        }
        if self.mqtt_url.is_empty() {
            return Err(ConfigError::Validation("mqtt_url cannot be empty".into()));
        }
        for sw in &self.switch {
            if sw.exec.is_none() && sw.dbus.is_none() {
                return Err(ConfigError::Validation(format!(
                    "Switch '{}' must have either 'exec' or 'dbus' defined",
                    sw.name
                )));
            }
            if sw.exec.is_some() && sw.dbus.is_some() {
                return Err(ConfigError::Validation(format!(
                    "Switch '{}' cannot have both 'exec' and 'dbus' defined",
                    sw.name
                )));
            }
        }
        Ok(())
    }

    // --- Derived topic helpers ---

    /// Base topic for device discovery: `homeassistant/device/{hostname}`
    pub fn device_base_topic(&self) -> String {
        format!("homeassistant/device/{}", self.hostname)
    }

    /// Discovery topic: `homeassistant/device/{hostname}/config`
    pub fn discovery_topic(&self) -> String {
        format!("{}/config", self.device_base_topic())
    }

    /// Status topic: `homeassistant/device/{hostname}/status`
    pub fn status_topic(&self) -> String {
        format!("{}/status", self.device_base_topic())
    }


}
