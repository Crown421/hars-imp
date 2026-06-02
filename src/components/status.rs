use async_trait::async_trait;

use crate::components::trait_def::Component;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusValue {
    On,
    Suspended,
    Off,
}

impl StatusValue {
    fn as_str(self) -> &'static str {
        match self {
            Self::On => "On",
            Self::Suspended => "Suspended",
            Self::Off => "Off",
        }
    }
}

/// A retained status sensor that exposes a user-visible device state in Home Assistant.
pub struct StatusComponent {
    display_name: String,
    discovery_key: String,
    state_topic: String,
}

impl StatusComponent {
    pub fn new(hostname: &str) -> Self {
        Self {
            display_name: format!("{hostname} Status"),
            discovery_key: format!("{hostname}_status"),
            state_topic: Self::state_topic_for(hostname),
        }
    }

    pub fn state_topic_for(hostname: &str) -> String {
        format!("homeassistant/sensor/{hostname}/status/state")
    }

    pub fn payload(status: StatusValue) -> String {
        format!(r#"{{"status":"{}"}}"#, status.as_str())
    }
}

#[async_trait]
impl Component for StatusComponent {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.display_name.clone(),
            unique_id: self.discovery_key.clone(),
            component_type: ComponentType::Sensor {
                state_topic: self.state_topic.clone(),
                device_class: None,
                unit_of_measurement: None,
                value_template: Some("{{ value_json.status }}".to_string()),
                icon: None,
            },
        }
    }

    fn discovery_components(&self) -> Vec<(String, HomeAssistantComponent)> {
        vec![(self.discovery_key.clone(), self.discovery_component())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::discovery::ComponentType;

    #[test]
    fn status_discovery_uses_old_sensor_contract() {
        let component = StatusComponent::new("myhost");
        let entries = component.discovery_components();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "myhost_status");

        let discovery = &entries[0].1;
        assert_eq!(discovery.unique_id, "myhost_status");
        assert_eq!(discovery.name, "myhost Status");

        match &discovery.component_type {
            ComponentType::Sensor {
                state_topic,
                device_class,
                unit_of_measurement,
                value_template,
                icon,
            } => {
                assert_eq!(state_topic, "homeassistant/sensor/myhost/status/state");
                assert_eq!(value_template.as_deref(), Some("{{ value_json.status }}"));
                assert!(device_class.is_none());
                assert!(unit_of_measurement.is_none());
                assert!(icon.is_none());
            }
            _ => panic!("expected sensor discovery"),
        }
    }

    #[test]
    fn status_payload_serializes_on() {
        assert_eq!(
            StatusComponent::payload(StatusValue::On),
            r#"{"status":"On"}"#
        );
    }

    #[test]
    fn status_payload_serializes_suspended() {
        assert_eq!(
            StatusComponent::payload(StatusValue::Suspended),
            r#"{"status":"Suspended"}"#
        );
    }

    #[test]
    fn status_payload_serializes_off() {
        assert_eq!(
            StatusComponent::payload(StatusValue::Off),
            r#"{"status":"Off"}"#
        );
    }
}
