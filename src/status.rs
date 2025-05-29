use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use tracing::{debug, info, warn};
use tokio::time::{timeout, Duration};
use crate::discovery::HomeAssistantComponent;
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
                     self.client.publish(&status_topic, QoS::AtLeastOnce, true, status_json)).await {
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

/// Creates status sensor component
pub fn create_status_component(config: &Config) -> (String, HomeAssistantComponent) {
    let component_id = format!("{}_status", config.hostname);
    let state_topic = format!("homeassistant/sensor/{}/status/state", config.hostname);
    
    let component = HomeAssistantComponent::sensor(
        format!("{} Status", config.hostname),
        component_id.clone(),
        state_topic,
        None, // device_class
        None, // unit_of_measurement
        "{{ value_json.status }}".to_string(),
    );
    
    (component_id, component)
}
