use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Debug)]
pub struct Button {
    pub name: String,
    pub exec: String,
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
    #[serde(skip)]
    pub sensor_topic_base: String,
    #[serde(skip)]
    pub button_topic: String,
    #[serde(skip)]
    pub device_discovery_topic: String,
}

impl Config {
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&contents)?;

        // Set derived fields after parsing
        config.sensor_topic_base = format!("homeassistant/sensor/{}", config.hostname);
        config.button_topic = format!("homeassistant/button/{}", config.hostname);
        config.device_discovery_topic = format!("homeassistant/device/{}/config", config.hostname);

        Ok(config)
    }
}
