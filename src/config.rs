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
    pub topics: Vec<String>,
    pub update_interval_ms: u64,
    pub button: Option<Vec<Button>>,
}

impl Config {
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }
}
