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

/// Component types that can be part of a device discovery
#[derive(Serialize, Clone)]
#[serde(tag = "p", rename_all = "lowercase")]
pub enum ComponentType {
    Button {
        #[serde(rename = "cmd_t")]
        command_topic: String,
    },
    Sensor {
        #[serde(rename = "stat_t")]
        state_topic: String,
        #[serde(rename = "dev_cla", skip_serializing_if = "Option::is_none")]
        device_class: Option<String>,
        #[serde(rename = "unit_of_meas", skip_serializing_if = "Option::is_none")]
        unit_of_measurement: Option<String>,
        #[serde(rename = "val_tpl")]
        value_template: String,
    },
    Switch {
        #[serde(rename = "cmd_t")]
        command_topic: String,
        #[serde(rename = "stat_t")]
        state_topic: String,
    },
    Notify {
        #[serde(rename = "cmd_t")]
        command_topic: String,
    },
}

/// A Home Assistant component with metadata
#[derive(Serialize, Clone)]
pub struct HomeAssistantComponent {
    pub name: String,
    pub unique_id: String,
    #[serde(flatten)]
    pub component_type: ComponentType,
}

impl HomeAssistantComponent {
    /// Create a new button component
    pub fn button(name: String, unique_id: String, command_topic: String) -> Self {
        Self {
            name,
            unique_id,
            component_type: ComponentType::Button { command_topic },
        }
    }

    /// Create a new sensor component
    pub fn sensor(
        name: String,
        unique_id: String,
        state_topic: String,
        device_class: Option<String>,
        unit_of_measurement: Option<String>,
        value_template: String,
    ) -> Self {
        Self {
            name,
            unique_id,
            component_type: ComponentType::Sensor {
                state_topic,
                device_class,
                unit_of_measurement,
                value_template,
            },
        }
    }

    /// Create a new switch component
    pub fn switch(
        name: String,
        unique_id: String,
        command_topic: String,
        state_topic: String,
    ) -> Self {
        Self {
            name,
            unique_id,
            component_type: ComponentType::Switch {
                command_topic,
                state_topic,
            },
        }
    }

    /// Create a new notify component
    pub fn notify(name: String, unique_id: String, command_topic: String) -> Self {
        Self {
            name,
            unique_id,
            component_type: ComponentType::Notify { command_topic },
        }
    }
}

/// Main device discovery payload
#[derive(Serialize)]
pub struct HomeAssistantDeviceDiscovery {
    #[serde(rename = "dev")]
    pub device: HomeAssistantDevice,
    #[serde(rename = "o")]
    pub origin: HomeAssistantOrigin,
    #[serde(rename = "cmps")]
    pub components: HashMap<String, HomeAssistantComponent>,
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

/// Creates a shared HomeAssistant origin object using version info
pub fn create_shared_origin() -> HomeAssistantOrigin {
    let version_info = VersionInfo::get();
    HomeAssistantOrigin {
        name: "MQTT Agent".to_string(),
        sw_version: version_info.version.clone(),
        support_url: version_info.repository.clone(),
    }
}

/// Builder for creating a complete device discovery with all components
pub struct DeviceDiscoveryBuilder {
    device: HomeAssistantDevice,
    origin: HomeAssistantOrigin,
    components: HashMap<String, HomeAssistantComponent>,
}

impl DeviceDiscoveryBuilder {
    /// Create a new builder with device and origin info
    pub fn new(config: &Config) -> Self {
        Self {
            device: create_shared_device(config),
            origin: create_shared_origin(),
            components: HashMap::new(),
        }
    }

    /// Add a component to the device
    pub fn add_component(
        mut self,
        component_id: String,
        component: HomeAssistantComponent,
    ) -> Self {
        self.components.insert(component_id, component);
        self
    }

    /// Add multiple components from an iterator
    pub fn add_components<I>(mut self, components: I) -> Self
    where
        I: IntoIterator<Item = (String, HomeAssistantComponent)>,
    {
        self.components.extend(components);
        self
    }

    /// Build the final device discovery payload
    pub fn build(self) -> HomeAssistantDeviceDiscovery {
        HomeAssistantDeviceDiscovery {
            device: self.device,
            origin: self.origin,
            components: self.components,
        }
    }
}

/// Publish unified device discovery with all components
pub async fn publish_unified_discovery(
    client: &AsyncClient,
    config: &Config,
    components: Vec<(String, HomeAssistantComponent)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let device_discovery = DeviceDiscoveryBuilder::new(config)
        .add_components(components)
        .build();

    debug!(
        "Publishing unified device discovery with {} components",
        device_discovery.components.len()
    );
    publish_discovery(
        client,
        &config.device_discovery_topic,
        &device_discovery,
        true,
    )
    .await?;

    Ok(())
}
