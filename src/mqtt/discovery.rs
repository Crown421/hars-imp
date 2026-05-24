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
    Button { command_topic: String },

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
    Notify { command_topic: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AmbientLightMonitorConfig;

    /// Helper: create a minimal Config for testing.
    fn test_config() -> Config {
        Config {
            hostname: "testhost".to_string(),
            mqtt_url: "mqtt.example.com".to_string(),
            mqtt_port: None,
            username: "user".to_string(),
            password: "pass".to_string(),
            log_level: "info".to_string(),
            update_interval_secs: 60,
            ambient_light_monitor: AmbientLightMonitorConfig::default(),
            button: vec![],
            switch: vec![],
            tls: None,
        }
    }

    #[test]
    fn button_component_serializes_correctly() {
        let component = HomeAssistantComponent {
            name: "Lock".to_string(),
            unique_id: "testhost_lock_button".to_string(),
            component_type: ComponentType::Button {
                command_topic: "homeassistant/button/testhost/lock/set".to_string(),
            },
        };

        let json = serde_json::to_value(&component).unwrap();
        assert_eq!(json["name"], "Lock");
        assert_eq!(json["unique_id"], "testhost_lock_button");
        assert_eq!(json["p"], "button");
        assert_eq!(
            json["command_topic"],
            "homeassistant/button/testhost/lock/set"
        );
    }

    #[test]
    fn sensor_component_serializes_with_optional_fields() {
        let component = HomeAssistantComponent {
            name: "CPU Usage".to_string(),
            unique_id: "testhost_cpu_usage".to_string(),
            component_type: ComponentType::Sensor {
                state_topic: "homeassistant/sensor/testhost/cpu_usage/state".to_string(),
                device_class: None,
                unit_of_measurement: Some("%".to_string()),
                value_template: None,
                icon: Some("mdi:cpu-64-bit".to_string()),
            },
        };

        let json = serde_json::to_value(&component).unwrap();
        assert_eq!(json["p"], "sensor");
        assert_eq!(json["unit_of_measurement"], "%");
        assert_eq!(json["icon"], "mdi:cpu-64-bit");
        // None fields should be absent
        assert!(json.get("device_class").is_none());
        assert!(json.get("value_template").is_none());
    }

    #[test]
    fn switch_component_serializes_with_both_topics() {
        let component = HomeAssistantComponent {
            name: "Night Light".to_string(),
            unique_id: "testhost_night_light_switch".to_string(),
            component_type: ComponentType::Switch {
                command_topic: "homeassistant/switch/testhost/night_light/set".to_string(),
                state_topic: "homeassistant/switch/testhost/night_light/state".to_string(),
            },
        };

        let json = serde_json::to_value(&component).unwrap();
        assert_eq!(json["p"], "switch");
        assert!(json.get("command_topic").is_some());
        assert!(json.get("state_topic").is_some());
    }

    #[test]
    fn notify_component_serializes() {
        let component = HomeAssistantComponent {
            name: "Notification".to_string(),
            unique_id: "testhost_notification".to_string(),
            component_type: ComponentType::Notify {
                command_topic: "homeassistant/notify/testhost".to_string(),
            },
        };

        let json = serde_json::to_value(&component).unwrap();
        assert_eq!(json["p"], "notify");
        assert_eq!(json["command_topic"], "homeassistant/notify/testhost");
    }

    #[test]
    fn builder_produces_valid_discovery() {
        let config = test_config();
        let discovery = DeviceDiscoveryBuilder::new(&config)
            .add_component(
                "lock_button".to_string(),
                HomeAssistantComponent {
                    name: "Lock".to_string(),
                    unique_id: "testhost_lock_button".to_string(),
                    component_type: ComponentType::Button {
                        command_topic: "homeassistant/button/testhost/lock/set".to_string(),
                    },
                },
            )
            .with_status_topic("homeassistant/device/testhost/status".to_string())
            .build();

        assert_eq!(discovery.dev.name, "testhost");
        assert_eq!(discovery.dev.identifiers, vec!["testhost"]);
        assert_eq!(discovery.cmps.len(), 1);
        assert!(discovery.cmps.contains_key("lock_button"));
        assert!(discovery.availability.is_some());
        assert_eq!(
            discovery.availability.as_ref().unwrap()[0].topic,
            "homeassistant/device/testhost/status"
        );
    }

    #[test]
    fn builder_no_status_topic_no_availability() {
        let config = test_config();
        let discovery = DeviceDiscoveryBuilder::new(&config).build();
        assert!(discovery.availability.is_none());
    }

    #[test]
    fn builder_add_multiple_components() {
        let config = test_config();
        let components = vec![
            (
                "btn1".to_string(),
                HomeAssistantComponent {
                    name: "Button 1".to_string(),
                    unique_id: "testhost_btn1".to_string(),
                    component_type: ComponentType::Button {
                        command_topic: "t1".to_string(),
                    },
                },
            ),
            (
                "btn2".to_string(),
                HomeAssistantComponent {
                    name: "Button 2".to_string(),
                    unique_id: "testhost_btn2".to_string(),
                    component_type: ComponentType::Button {
                        command_topic: "t2".to_string(),
                    },
                },
            ),
        ];

        let discovery = DeviceDiscoveryBuilder::new(&config)
            .add_components(components)
            .build();

        assert_eq!(discovery.cmps.len(), 2);
        assert!(discovery.cmps.contains_key("btn1"));
        assert!(discovery.cmps.contains_key("btn2"));
    }

    #[test]
    fn full_discovery_json_round_trip() {
        let config = test_config();
        let discovery = DeviceDiscoveryBuilder::new(&config)
            .add_component(
                "cpu".to_string(),
                HomeAssistantComponent {
                    name: "CPU Usage".to_string(),
                    unique_id: "testhost_cpu_usage".to_string(),
                    component_type: ComponentType::Sensor {
                        state_topic: "homeassistant/sensor/testhost/cpu_usage/state".to_string(),
                        device_class: None,
                        unit_of_measurement: Some("%".to_string()),
                        value_template: None,
                        icon: Some("mdi:cpu-64-bit".to_string()),
                    },
                },
            )
            .with_status_topic("homeassistant/device/testhost/status".to_string())
            .build();

        // Serialize to JSON string
        let json_str = serde_json::to_string(&discovery).expect("serialization should succeed");

        // Parse back to a generic Value to verify structure
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("should parse back");

        // Verify top-level structure
        assert!(parsed.get("dev").is_some(), "should have 'dev' key");
        assert!(parsed.get("o").is_some(), "should have 'o' (origin) key");
        assert!(parsed.get("cmps").is_some(), "should have 'cmps' key");
        assert!(
            parsed.get("availability").is_some(),
            "should have 'availability' key"
        );

        // Verify device info
        assert_eq!(parsed["dev"]["name"], "testhost");
        assert_eq!(parsed["dev"]["identifiers"][0], "testhost");

        // Verify origin
        assert_eq!(parsed["o"]["name"], "hars-imp");

        // Verify component
        let cpu = &parsed["cmps"]["cpu"];
        assert_eq!(cpu["p"], "sensor");
        assert_eq!(cpu["unit_of_measurement"], "%");
    }

    #[test]
    fn device_info_optional_fields_serialized() {
        let info = DeviceInfo {
            identifiers: vec!["host1".to_string()],
            name: "Host 1".to_string(),
            model: None,
            manufacturer: None,
            sw_version: None,
        };

        let json = serde_json::to_value(&info).unwrap();
        // None fields should be absent due to skip_serializing_if
        assert!(json.get("model").is_none());
        assert!(json.get("manufacturer").is_none());
        assert!(json.get("sw_version").is_none());
    }
}
