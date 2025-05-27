use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use std::collections::HashMap;
use tracing::{debug, info};
use crate::config::Config;
use crate::system_monitor::SYSTEM_METRICS;

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
    #[serde(rename = "unit_of_measurement", skip_serializing_if = "Option::is_none")]
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
fn create_shared_device(config: &Config) -> HomeAssistantDevice {
    HomeAssistantDevice {
        identifiers: config.hostname.clone(),
        name: config.hostname.clone(),
        model: "MQTT Daemon".to_string(),
        manufacturer: "Custom".to_string(),
        sw_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub async fn setup_button_discovery(
    client: &AsyncClient,
    config: &Config,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let mut button_topics = Vec::new();
    
    if let Some(buttons) = &config.button {
        debug!("Setting up {} button(s)", buttons.len());
        for button in buttons {
            let button_topic = format!("homeassistant/button/{}/set", 
                                     format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()));
            let discovery_topic = format!("homeassistant/button/{}/config", 
                                        format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()));
            
            // Create discovery message
            let discovery_message = HomeAssistantDiscovery {
                name: button.name.clone(),
                command_topic: button_topic.clone(),
                unique_id: format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()),
                device: create_shared_device(config),
            };
            
            let discovery_json = serde_json::to_string(&discovery_message)?;
            
            // Publish discovery message
            info!("Publishing discovery for button '{}' to: {}", button.name, discovery_topic);
            debug!("Discovery payload: {}", discovery_json);
            client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;
            
            // Subscribe to button command topic
            info!("Subscribing to button topic: {}", button_topic);
            client.subscribe(&button_topic, QoS::AtMostOnce).await?;
            
            button_topics.push((button_topic, button.exec.clone()));
        }
    }
    
    Ok(button_topics)
}

pub async fn setup_sensor_discovery(
    client: &AsyncClient,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = create_shared_device(config);

    let origin = HomeAssistantOrigin {
        name: "MQTT Agent".to_string(),
        sw_version: env!("CARGO_PKG_VERSION").to_string(),
        support_url: "https://github.com/your-repo/mqtt-agent".to_string(),
    };

    // Create sensor components from system metrics configuration
    let mut components = HashMap::new();
    let state_topic = format!("homeassistant/sensor/{}/system_performance/state", config.hostname);
    
    for metric in SYSTEM_METRICS {
        let component_id = format!("{}_{}", config.hostname, metric.json_field);
        let component = HomeAssistantComponent {
            name: format!("{} {}", config.hostname, metric.name),
            platform: "sensor".to_string(),
            device_class: metric.device_class.map(|s| s.to_string()),
            unit_of_measurement: metric.unit.map(|s| s.to_string()),
            value_template: format!("{{{{ value_json.{} }}}}", metric.json_field),
            unique_id: component_id.clone(),
        };
        components.insert(component_id, component);
    }

    // Create device discovery payload
    let device_discovery = HomeAssistantDeviceDiscovery {
        device: device.clone(),
        origin,
        components,
        state_topic,
    };

    // Publish single device discovery message
    let discovery_topic = format!("homeassistant/device/{}/config", config.hostname);
    let discovery_json = serde_json::to_string(&device_discovery)?;
    
    info!("Publishing device discovery for '{}' to: {}", config.hostname, discovery_topic);
    info!("Device discovery payload: {}", discovery_json);
    client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;

    Ok(())
}

pub async fn setup_status_discovery(
    client: &AsyncClient,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = create_shared_device(config);

    // Create status sensor discovery
    let status_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} Status", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/status/state", config.hostname),
        unique_id: format!("{}_status", config.hostname),
        device_class: None,
        unit_of_measurement: None,
        value_template: "{{ value_json.status }}".to_string(),
        device,
    };

    // Publish status sensor discovery message
    let discovery_topic = format!("homeassistant/sensor/{}_status/config", config.hostname);
    let discovery_json = serde_json::to_string(&status_discovery)?;
    
    info!("Publishing status sensor discovery for '{}' to: {}", config.hostname, discovery_topic);
    debug!("Status discovery payload: {}", discovery_json);
    client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;

    Ok(())
}
