use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use tracing::{debug, info};
use crate::config::Config;

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
    pub value_template: Option<String>,
    pub device: HomeAssistantDevice,
}

#[derive(Serialize, Clone)]
pub struct HomeAssistantDevice {
    pub identifiers: Vec<String>,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
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
                device: HomeAssistantDevice {
                    identifiers: vec![config.hostname.clone()],
                    name: config.hostname.clone(),
                    model: "MQTT Daemon".to_string(),
                    manufacturer: "Custom".to_string(),
                },
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
    let device = HomeAssistantDevice {
        identifiers: vec![config.hostname.clone()],
        name: config.hostname.clone(),
        model: "MQTT Daemon".to_string(),
        manufacturer: "Custom".to_string(),
    };

    // CPU Load sensor
    let cpu_load_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} CPU Load", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/cpu_load/state", config.hostname),
        unique_id: format!("{}_cpu_load", config.hostname),
        device_class: None,
        unit_of_measurement: Some("%".to_string()),
        value_template: Some("{{ value_json.load }}".to_string()),
        device: device.clone(),
    };

    // CPU Frequency sensor
    let cpu_freq_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} CPU Frequency", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/cpu_frequency/state", config.hostname),
        unique_id: format!("{}_cpu_frequency", config.hostname),
        device_class: None,
        unit_of_measurement: Some("MHz".to_string()),
        value_template: Some("{{ value_json.frequency }}".to_string()),
        device: device.clone(),
    };

    // Memory Total sensor
    let memory_total_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} Memory Total", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/memory_total/state", config.hostname),
        unique_id: format!("{}_memory_total", config.hostname),
        device_class: Some("data_size".to_string()),
        unit_of_measurement: Some("GB".to_string()),
        value_template: Some("{{ value_json.total }}".to_string()),
        device: device.clone(),
    };

    // Memory Free sensor
    let memory_free_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} Memory Free", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/memory_free/state", config.hostname),
        unique_id: format!("{}_memory_free", config.hostname),
        device_class: Some("data_size".to_string()),
        unit_of_measurement: Some("GB".to_string()),
        value_template: Some("{{ value_json.free }}".to_string()),
        device: device.clone(),
    };

    // Memory Free Percentage sensor
    let memory_free_pct_discovery = HomeAssistantSensorDiscovery {
        name: format!("{} Memory Free %", config.hostname),
        state_topic: format!("homeassistant/sensor/{}/memory_free_pct/state", config.hostname),
        unique_id: format!("{}_memory_free_pct", config.hostname),
        device_class: None,
        unit_of_measurement: Some("%".to_string()),
        value_template: Some("{{ value_json.free_percentage }}".to_string()),
        device: device,
    };

    // Publish all sensor discoveries
    let sensors = vec![
        (cpu_load_discovery, format!("homeassistant/sensor/{}/cpu_load/config", config.hostname)),
        (cpu_freq_discovery, format!("homeassistant/sensor/{}/cpu_frequency/config", config.hostname)),
        (memory_total_discovery, format!("homeassistant/sensor/{}/memory_total/config", config.hostname)),
        (memory_free_discovery, format!("homeassistant/sensor/{}/memory_free/config", config.hostname)),
        (memory_free_pct_discovery, format!("homeassistant/sensor/{}/memory_free_pct/config", config.hostname)),
    ];

    for (sensor, topic) in sensors {
        let discovery_json = serde_json::to_string(&sensor)?;
        info!("Publishing sensor discovery to: {}", topic);
        debug!("Sensor discovery payload: {}", discovery_json);
        client.publish(&topic, QoS::AtLeastOnce, true, discovery_json).await?;
    }

    Ok(())
}
