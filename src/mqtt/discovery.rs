use std::collections::HashMap;

use serde::Serialize;

use crate::config::Config;
use crate::util::version;

/// A single HA entity's discovery configuration.
///
/// This is the value in the `cmps` map of the device discovery payload.
#[derive(Debug, Clone, Serialize)]
pub struct HomeAssistantComponent {
    /// Display name in HA.
    pub name: String,

    /// Unique ID for HA entity registry.
    pub unique_id: String,

    /// Component-type-specific fields (flattened into the JSON).
    #[serde(flatten)]
    pub component_type: ComponentType,
}

/// Type-specific fields for each HA platform.
///
/// The `p` field (platform) is the HA component type identifier.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "p", rename_all = "lowercase")]
pub enum ComponentType {
    /// A button entity — receives press commands.
    Button {
        command_topic: String,
    },

    /// A sensor entity — publishes state values.
    Sensor {
        state_topic: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_class: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit_of_measurement: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        value_template: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        icon: Option<String>,
    },

    /// A switch entity — receives on/off commands, publishes state.
    Switch {
        command_topic: String,
        state_topic: String,
    },

    /// A notification entity — receives notification payloads.
    Notify {
        command_topic: String,
    },
}

/// The top-level HA device discovery v2 payload.
///
/// Published as a single retained message to `homeassistant/device/{id}/config`.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceDiscovery {
    /// Device metadata.
    pub dev: DeviceInfo,

    /// Origin (app) metadata.
    pub o: OriginInfo,

    /// Map of component_key → component config.
    pub cmps: HashMap<String, HomeAssistantComponent>,

    /// Availability configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<Vec<AvailabilityEntry>>,
}

/// Device identification for HA.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    /// List of device identifiers.
    pub identifiers: Vec<String>,

    /// Device name.
    pub name: String,

    /// Device model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Manufacturer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,

    /// Software version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sw_version: Option<String>,
}

/// Origin metadata — identifies this application to HA.
#[derive(Debug, Clone, Serialize)]
pub struct OriginInfo {
    /// Application name.
    pub name: String,

    /// Application version.
    pub sw_version: String,

    /// Repository URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// An availability topic entry for HA.
#[derive(Debug, Clone, Serialize)]
pub struct AvailabilityEntry {
    pub topic: String,
}

/// Builder for constructing the full device discovery payload.
pub struct DeviceDiscoveryBuilder {
    device: DeviceInfo,
    origin: OriginInfo,
    components: HashMap<String, HomeAssistantComponent>,
    status_topic: Option<String>,
}

impl DeviceDiscoveryBuilder {
    /// Create a new builder from application config.
    pub fn new(config: &Config) -> Self {
        Self {
            device: DeviceInfo {
                identifiers: vec![config.hostname.clone()],
                name: config.hostname.clone(),
                model: Some("PC".to_string()),
                manufacturer: Some("Linux".to_string()),
                sw_version: Some(version::APP_VERSION.to_string()),
            },
            origin: OriginInfo {
                name: version::APP_NAME.to_string(),
                sw_version: version::APP_VERSION.to_string(),
                url: Some(version::APP_REPOSITORY.to_string()),
            },
            components: HashMap::new(),
            status_topic: None,
        }
    }

    /// Add a single component to the discovery payload.
    #[allow(dead_code)]
    pub fn add_component(mut self, key: String, component: HomeAssistantComponent) -> Self {
        self.components.insert(key, component);
        self
    }

    /// Add multiple components at once.
    pub fn add_components(
        mut self,
        components: impl IntoIterator<Item = (String, HomeAssistantComponent)>,
    ) -> Self {
        self.components.extend(components);
        self
    }

    /// Set the availability/status topic.
    pub fn with_status_topic(mut self, topic: String) -> Self {
        self.status_topic = Some(topic);
        self
    }

    /// Build the final discovery payload.
    pub fn build(self) -> DeviceDiscovery {
        let availability = self
            .status_topic
            .map(|topic| vec![AvailabilityEntry { topic }]);

        DeviceDiscovery {
            dev: self.device,
            o: self.origin,
            cmps: self.components,
            availability,
        }
    }
}
