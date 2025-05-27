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

#[derive(Debug, Clone)]
struct SensorConfig {
    pub name: &'static str,
    pub sensor_type: &'static str,
    pub device_class: Option<&'static str>,
    pub unit_of_measurement: Option<&'static str>,
    pub value_template: &'static str,
}

impl SensorConfig {
    const fn new(
        name: &'static str,
        sensor_type: &'static str,
        device_class: Option<&'static str>,
        unit_of_measurement: Option<&'static str>,
        value_template: &'static str,
    ) -> Self {
        Self {
            name,
            sensor_type,
            device_class,
            unit_of_measurement,
            value_template,
        }
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

    // Helper function to create sensor discovery config
    let create_sensor = |sensor_config: &SensorConfig| -> HomeAssistantSensorDiscovery {
        HomeAssistantSensorDiscovery {
            name: format!("{} {}", config.hostname, sensor_config.name),
            state_topic: format!("homeassistant/sensor/{}/{}/state", config.hostname, sensor_config.sensor_type),
            unique_id: format!("{}_{}", config.hostname, sensor_config.sensor_type),
            device_class: sensor_config.device_class.map(|s| s.to_string()),
            unit_of_measurement: sensor_config.unit_of_measurement.map(|s| s.to_string()),
            value_template: Some(sensor_config.value_template.to_string()),
            device: device.clone(),
        }
    };

    // Define sensor configurations using structured approach
    const SENSOR_CONFIGS: &[SensorConfig] = &[
        SensorConfig::new(
            "CPU Load",
            "cpu_load",
            None,
            Some("%"),
            "{{ value_json.load }}"
        ),
        SensorConfig::new(
            "CPU Frequency",
            "cpu_frequency",
            None,
            Some("GHz"),
            "{{ value_json.frequency }}"
        ),
        SensorConfig::new(
            "Memory Total",
            "memory_total",
            Some("data_size"),
            Some("GB"),
            "{{ value_json.total }}"
        ),
        SensorConfig::new(
            "Memory Free",
            "memory_free",
            Some("data_size"),
            Some("GB"),
            "{{ value_json.free }}"
        ),
        SensorConfig::new(
            "Memory Free %",
            "memory_free_pct",
            None,
            Some("%"),
            "{{ value_json.free_percentage }}"
        ),
    ];

    // Create and publish sensor discoveries
    for sensor_config in SENSOR_CONFIGS {
        let sensor_discovery = create_sensor(sensor_config);
        let discovery_topic = format!("homeassistant/sensor/{}/{}/config", config.hostname, sensor_config.sensor_type);
        
        let discovery_json = serde_json::to_string(&sensor_discovery)?;
        info!("Publishing sensor discovery for '{}' to: {}", sensor_config.name, discovery_topic);
        debug!("Sensor discovery payload: {}", discovery_json);
        client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;
    }

    Ok(())
}
