use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Debug)]
pub struct Button {
    pub name: String,
    pub exec: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DBusAction {
    pub service: String,
    pub path: String,
    pub interface: String,
    pub method: String,
}

#[derive(Deserialize, Debug)]
pub struct Switch {
    pub name: String,
    pub exec: Option<String>,
    pub dbus: Option<DBusAction>,
}

#[derive(Deserialize, Debug)]
pub struct Config {
    pub hostname: String,
    pub mqtt_url: String,
    pub mqtt_port: u16,
    pub username: String,
    pub password: String,
    pub log_level: String,
    pub update_interval_ms: u64,
    pub button: Option<Vec<Button>>,
    pub switch: Option<Vec<Switch>>,
    #[serde(skip)]
    pub sensor_topic_base: String,
    #[serde(skip)]
    pub button_topic: String,
    #[serde(skip)]
    pub device_discovery_topic: String,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = Self::get_config_path()?;
        Self::load_from_file(&config_path)
    }

    pub fn get_config_path() -> Result<String, Box<dyn std::error::Error>> {
        #[cfg(debug_assertions)]
        {
            // In debug mode, look for config.toml in the current directory
            Ok("config.toml".to_string())
        }

        #[cfg(not(debug_assertions))]
        {
            // In release mode, look for config.toml in $HOME/.config/hars-imp
            let home = std::env::var("HOME").map_err(|_| "HOME environment variable not set")?;
            let config_path = format!("{}/.config/hars-imp/config.toml", home);

            Ok(config_path)
        }
    }

    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&contents)?;

        // Validate switch configurations
        if let Some(switches) = &config.switch {
            for switch in switches {
                switch
                    .validate()
                    .map_err(|e| format!("Configuration error: {}", e))?;
            }
        }

        // Set derived fields after parsing
        config.sensor_topic_base = format!("homeassistant/sensor/{}", config.hostname);
        config.button_topic = format!("homeassistant/button/{}", config.hostname);
        config.device_discovery_topic = format!("homeassistant/device/{}/config", config.hostname);

        Ok(config)
    }
}

impl Switch {
    /// Validates that exactly one action type (exec or dbus) is specified
    pub fn validate(&self) -> Result<(), String> {
        match (&self.exec, &self.dbus) {
            (Some(_), Some(_)) => Err(format!(
                "Switch '{}' cannot have both 'exec' and 'dbus' actions. Please specify only one.",
                self.name
            )),
            (None, None) => Err(format!(
                "Switch '{}' must have either 'exec' or 'dbus' action specified.",
                self.name
            )),
            _ => Ok(()),
        }
    }

    /// Returns the action type for this switch
    pub fn action_type(&self) -> SwitchActionType {
        if self.exec.is_some() {
            SwitchActionType::Exec
        } else {
            SwitchActionType::DBus
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SwitchActionType {
    Exec,
    DBus,
}
