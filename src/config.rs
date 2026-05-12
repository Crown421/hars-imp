use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::info;

use crate::error::ConfigError;
use crate::util::helpers::slugify;

/// Top-level application configuration, loaded from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Hostname used as the device identifier in HA.
    pub hostname: String,

    /// MQTT broker address.
    pub mqtt_url: String,

    /// MQTT broker port.
    ///
    /// `None` means the user omitted the port, allowing TLS configs to default
    /// to 8883 while non-TLS configs default to 1883.
    #[serde(default)]
    pub mqtt_port: Option<u16>,

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

    /// TLS configuration.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// TLS configuration for the MQTT connection.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    /// Path to a PEM-encoded CA certificate file.
    /// If omitted, the system's native root certificates are used.
    pub ca_file: Option<String>,

    /// Path to a PEM-encoded client certificate file (for mTLS).
    pub client_cert: Option<String>,

    /// Path to a PEM-encoded client private key file (for mTLS).
    pub client_key: Option<String>,
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

fn default_mqtt_tls_port() -> u16 {
    8883
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
        if self.hostname.trim().is_empty() {
            return Err(ConfigError::Validation("hostname cannot be empty".into()));
        }
        if self.mqtt_url.trim().is_empty() {
            return Err(ConfigError::Validation("mqtt_url cannot be empty".into()));
        }

        let mut component_slugs = HashSet::new();
        for btn in &self.button {
            validate_component_name("button", &btn.name, &mut component_slugs)?;
        }

        for sw in &self.switch {
            validate_component_name("switch", &sw.name, &mut component_slugs)?;
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
        // Validate TLS mutual auth config: both cert and key must be present together.
        if let Some(ref tls) = self.tls {
            let has_cert = tls.client_cert.is_some();
            let has_key = tls.client_key.is_some();
            if has_cert != has_key {
                return Err(ConfigError::Validation(
                    "TLS client_cert and client_key must both be specified for mTLS".into(),
                ));
            }
        }

        Ok(())
    }

    /// Returns the effective MQTT port.
    ///
    /// If the user omitted `mqtt_port`, this returns 8883 when TLS is enabled
    /// and 1883 otherwise. Explicit ports are always preserved as-is.
    pub fn effective_mqtt_port(&self) -> u16 {
        match self.mqtt_port {
            Some(port) => port,
            None if self.tls.is_some() => default_mqtt_tls_port(),
            None => default_mqtt_port(),
        }
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

fn validate_component_name(
    kind: &str,
    name: &str,
    slugs: &mut HashSet<String>,
) -> Result<(), ConfigError> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{kind} name cannot be empty"
        )));
    }

    let slug = slugify(trimmed_name);
    if slug.is_empty() {
        return Err(ConfigError::Validation(format!(
            "{kind} '{name}' must contain at least one alphanumeric character"
        )));
    }

    if !slugs.insert(slug.clone()) {
        return Err(ConfigError::Validation(format!(
            "Duplicate component slug '{slug}' from {kind} '{name}'"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write TOML to a temp file and load it.
    fn load_toml(toml_content: &str) -> Result<Config, crate::error::ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(toml_content.as_bytes()).unwrap();
        Config::load_from(&path)
    }

    const MINIMAL_CONFIG: &str = r#"
hostname = "testhost"
mqtt_url = "mqtt.example.com"
username = "user"
password = "pass"
"#;

    #[test]
    fn parse_minimal_config() {
        let config = load_toml(MINIMAL_CONFIG).expect("should parse");
        assert_eq!(config.hostname, "testhost");
        assert_eq!(config.mqtt_url, "mqtt.example.com");
        assert_eq!(config.username, "user");
        assert_eq!(config.password, "pass");
    }

    #[test]
    fn defaults_applied() {
        let config = load_toml(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.mqtt_port, None);
        assert_eq!(config.effective_mqtt_port(), 1883);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.update_interval_secs, 60);
        assert!(config.button.is_empty());
        assert!(config.switch.is_empty());
        assert!(config.tls.is_none());
    }

    #[test]
    fn override_defaults() {
        let toml = r#"
hostname = "mypc"
mqtt_url = "broker"
username = "u"
password = "p"
mqtt_port = 9999
log_level = "debug"
update_interval_secs = 10
"#;
        let config = load_toml(toml).unwrap();
        assert_eq!(config.mqtt_port, Some(9999));
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.update_interval_secs, 10);
    }

    #[test]
    fn validation_empty_hostname() {
        let toml = r#"
hostname = ""
mqtt_url = "broker"
username = "u"
password = "p"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("hostname"),
            "Error should mention hostname: {msg}"
        );
    }

    #[test]
    fn validation_empty_mqtt_url() {
        let toml = r#"
hostname = "host"
mqtt_url = ""
username = "u"
password = "p"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mqtt_url"),
            "Error should mention mqtt_url: {msg}"
        );
    }

    #[test]
    fn validation_switch_needs_action() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[switch]]
name = "Bad Switch"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exec") || msg.contains("dbus"),
            "Error should mention missing action: {msg}"
        );
    }

    #[test]
    fn validation_switch_both_actions_rejected() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[switch]]
name = "Bad Switch"
exec = "echo hi"
[switch.dbus]
service = "org.test"
path = "/test"
interface = "org.test.iface"
method = "Toggle"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("both"), "Error should mention both: {msg}");
    }

    #[test]
    fn validation_button_name_cannot_be_empty() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[button]]
name = "   "
exec = "echo hi"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("button name"),
            "Error should mention button name: {msg}"
        );
    }

    #[test]
    fn validation_component_name_must_slugify() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[button]]
name = "---"
exec = "echo hi"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("alphanumeric"),
            "Error should mention alphanumeric content: {msg}"
        );
    }

    #[test]
    fn validation_duplicate_component_slugs_rejected() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[button]]
name = "Night Light"
exec = "echo hi"

[[switch]]
name = "Night-Light"
exec = "true"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Duplicate component slug"),
            "Error should mention duplicate slug: {msg}"
        );
    }

    #[test]
    fn parse_buttons_and_switches() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[[button]]
name = "Lock"
exec = "loginctl lock-session"

[[button]]
name = "Reboot"
exec = "systemctl reboot"

[[switch]]
name = "Night Light"
exec = "toggle-nightlight"
"#;
        let config = load_toml(toml).unwrap();
        assert_eq!(config.button.len(), 2);
        assert_eq!(config.button[0].name, "Lock");
        assert_eq!(config.button[1].exec, "systemctl reboot");
        assert_eq!(config.switch.len(), 1);
        assert_eq!(config.switch[0].name, "Night Light");
    }

    #[test]
    fn tls_config_parsing() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[tls]
ca_file = "/etc/ssl/ca.pem"
client_cert = "/etc/ssl/client.pem"
client_key = "/etc/ssl/client.key"
"#;
        let config = load_toml(toml).unwrap();
        let tls = config.tls.as_ref().unwrap();
        assert_eq!(tls.ca_file.as_deref(), Some("/etc/ssl/ca.pem"));
        assert_eq!(tls.client_cert.as_deref(), Some("/etc/ssl/client.pem"));
        assert_eq!(tls.client_key.as_deref(), Some("/etc/ssl/client.key"));
    }

    #[test]
    fn tls_mtls_requires_both_cert_and_key() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[tls]
client_cert = "/etc/ssl/client.pem"
"#;
        let err = load_toml(toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("client_cert") || msg.contains("client_key"),
            "Error should mention mTLS: {msg}"
        );
    }

    #[test]
    fn effective_mqtt_port_default_no_tls() {
        let config = load_toml(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.effective_mqtt_port(), 1883);
    }

    #[test]
    fn effective_mqtt_port_auto_tls() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"

[tls]
"#;
        let config = load_toml(toml).unwrap();
        // TLS enabled, port at default → should auto-select 8883
        assert_eq!(config.effective_mqtt_port(), 8883);
    }

    #[test]
    fn effective_mqtt_port_explicit_with_tls() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"
mqtt_port = 9999

[tls]
"#;
        let config = load_toml(toml).unwrap();
        // Explicitly set port overrides auto-detection
        assert_eq!(config.effective_mqtt_port(), 9999);
    }

    #[test]
    fn effective_mqtt_port_explicit_1883_with_tls() {
        let toml = r#"
hostname = "host"
mqtt_url = "broker"
username = "u"
password = "p"
mqtt_port = 1883

[tls]
"#;
        let config = load_toml(toml).unwrap();
        assert_eq!(config.mqtt_port, Some(1883));
        assert_eq!(config.effective_mqtt_port(), 1883);
    }

    #[test]
    fn derived_topic_helpers() {
        let config = load_toml(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.device_base_topic(), "homeassistant/device/testhost");
        assert_eq!(
            config.discovery_topic(),
            "homeassistant/device/testhost/config"
        );
        assert_eq!(
            config.status_topic(),
            "homeassistant/device/testhost/status"
        );
    }

    #[test]
    fn missing_required_field_is_parse_error() {
        let toml = r#"
hostname = "host"
"#;
        let err = load_toml(toml).unwrap_err();
        // Should be a Parse error, not a Validation error
        assert!(
            matches!(err, crate::error::ConfigError::Parse(_)),
            "Expected Parse error, got: {err}"
        );
    }

    #[test]
    fn invalid_toml_syntax() {
        let toml = "this is not valid toml [[[";
        let err = load_toml(toml).unwrap_err();
        assert!(
            matches!(err, crate::error::ConfigError::Parse(_)),
            "Expected Parse error, got: {err}"
        );
    }
}
