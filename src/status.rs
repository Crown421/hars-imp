use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use tracing::{debug, info, warn};
use tokio::time::{timeout, Duration};
use crate::discovery::{publish_discovery, create_shared_device, HomeAssistantSensorDiscovery};
use crate::utils::Config;

#[derive(Serialize)]
struct StatusData {
    status: String,
}

pub struct StatusManager {
    hostname: String,
    client: AsyncClient,
}

impl StatusManager {
    pub fn new(hostname: String, client: AsyncClient) -> Self {
        Self { hostname, client }
    }

    pub async fn publish_status(&self, status: &str) -> Result<(), Box<dyn std::error::Error>> {
        let status_data = StatusData { 
            status: status.to_string() 
        };
        let status_json = serde_json::to_string(&status_data)?;
        let status_topic = format!("homeassistant/sensor/{}/status/state", self.hostname);
        
        info!("Publishing status: {}", status);
        
        match timeout(Duration::from_secs(5), 
                     self.client.publish(&status_topic, QoS::AtMostOnce, false, status_json)).await {
            Ok(result) => result?,
            Err(_) => {
                warn!("Timeout publishing status '{}' to topic '{}'", status, status_topic);
                return Err("Timeout publishing status".into());
            }
        }
        
        debug!("Successfully published status: {}", status);
        Ok(())
    }

    pub async fn publish_on(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.publish_status("On").await
    }

    pub async fn publish_off(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.publish_status("Off").await
    }

    pub async fn publish_suspended(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.publish_status("Suspended").await
    }
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
    
    info!("Publishing status sensor discovery for '{}'", config.hostname);
    publish_discovery(client, &discovery_topic, &status_discovery, true).await?;

    Ok(())
}
