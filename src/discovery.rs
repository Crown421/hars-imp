use crate::utils::{Config, VersionInfo};
use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use std::collections::HashMap;
use tracing::debug;

/// Generic function to publish Home Assistant discovery messages
pub async fn publish_discovery<T: Serialize>(
    client: &AsyncClient,
    discovery_topic: &str,
    discovery_payload: &T,
    retain: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let discovery_json = serde_json::to_string(discovery_payload)?;

    debug!("Publishing discovery to: {}", discovery_topic);
    debug!("Discovery payload: {}", discovery_json);
    client
        .publish(discovery_topic, QoS::AtLeastOnce, retain, discovery_json)
        .await?;

    Ok(())
}

#[derive(Serialize)]
pub struct HomeAssistantDiscovery {
    pub name: String,
    pub command_topic: String,
    pub unique_id: String,
    pub device: HomeAssistantDevice,
}

#[derive(Serialize)]
pub struct HomeAssistantSensorDiscovery {
    pub name: String,
    pub state_topic: String,
    pub unique_id: String,
    pub device_class: Option<String>,
    pub unit_of_measurement: Option<String>,
    pub value_template: String,
    pub device: HomeAssistantDevice,
}

#[derive(Serialize)]
pub struct HomeAssistantDeviceDiscovery {
    #[serde(rename = "dev")]
    pub device: HomeAssistantDevice,
    #[serde(rename = "o")]
    pub origin: HomeAssistantOrigin,
    #[serde(rename = "cmps")]
    pub components: HashMap<String, HomeAssistantComponent>,
    pub state_topic: String,
}

#[derive(Serialize)]
pub struct HomeAssistantComponent {
    pub name: String,
    #[serde(rename = "p")]
    pub platform: String,
    #[serde(rename = "device_class", skip_serializing_if = "Option::is_none")]
    pub device_class: Option<String>,
    #[serde(
        rename = "unit_of_measurement",
        skip_serializing_if = "Option::is_none"
    )]
    pub unit_of_measurement: Option<String>,
    pub value_template: String,
    pub unique_id: String,
}

#[derive(Serialize)]
pub struct HomeAssistantOrigin {
    pub name: String,
    #[serde(rename = "sw")]
    pub sw_version: String,
    #[serde(rename = "url")]
    pub support_url: String,
}

#[derive(Serialize, Clone)]
pub struct HomeAssistantDevice {
    #[serde(rename = "ids")]
    pub identifiers: String,
    pub name: String,
    #[serde(rename = "mdl")]
    pub model: String,
    #[serde(rename = "mf")]
    pub manufacturer: String,
    #[serde(rename = "sw")]
    pub sw_version: String,
}

/// Creates a shared HomeAssistant device object using the hostname from config
/// and the version from Cargo.toml at compile time
pub fn create_shared_device(config: &Config) -> HomeAssistantDevice {
    let version_info = VersionInfo::get();
    HomeAssistantDevice {
        identifiers: config.hostname.clone(),
        name: config.hostname.clone(),
        model: "MQTT Daemon".to_string(),
        manufacturer: "Custom".to_string(),
        sw_version: version_info.version.clone(),
    }
}
