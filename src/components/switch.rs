use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::components::trait_def::{ActionMessage, Component, OutboundMessage};
use crate::config::{DbusActionConfig, SwitchConfig};
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};
use crate::util::helpers::{execute_command, slugify};

/// The action a switch performs: either a shell command or a D-Bus method call.
#[derive(Debug, Clone)]
enum SwitchAction {
    Exec(String),
    Dbus(DbusActionConfig),
}

/// A switch entity with ON/OFF state.
pub struct SwitchComponent {
    name: String,
    unique_id: String,
    command_topic: String,
    state_topic: String,
    action: SwitchAction,

    /// Current state, protected for interior mutability.
    state: Mutex<bool>,

    /// Serializes command handling so rapid ON/OFF messages cannot complete out of order.
    action_lock: Mutex<()>,
}

impl SwitchComponent {
    /// Create a switch component from config.
    pub fn new(config: &SwitchConfig, hostname: &str) -> Self {
        let slug = slugify(&config.name);
        let action = if let Some(ref exec) = config.exec {
            SwitchAction::Exec(exec.clone())
        } else if let Some(ref dbus) = config.dbus {
            SwitchAction::Dbus(dbus.clone())
        } else {
            // Validation in Config::validate() prevents this, but just in case.
            panic!("Switch '{}' has no action defined", config.name);
        };

        Self {
            name: config.name.clone(),
            unique_id: format!("{hostname}_{slug}_switch"),
            command_topic: format!("homeassistant/switch/{hostname}/{slug}/set"),
            state_topic: format!("homeassistant/switch/{hostname}/{slug}/state"),
            action,
            state: Mutex::new(false),
            action_lock: Mutex::new(()),
        }
    }

    /// Publish the current state to the state topic.
    async fn publish_state(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        let state = *self.state.lock().await;
        let payload = if state { "ON" } else { "OFF" };
        let _ = action_tx
            .send(OutboundMessage::state(self.state_topic.clone(), payload))
            .await;
    }
}

#[async_trait]
impl Component for SwitchComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.name.clone(),
            unique_id: self.unique_id.clone(),
            component_type: ComponentType::Switch {
                command_topic: self.command_topic.clone(),
                state_topic: self.state_topic.clone(),
            },
        }
    }

    fn subscriptions(&self) -> Vec<String> {
        vec![self.command_topic.clone()]
    }

    async fn handle_message(
        &self,
        _topic: &str,
        payload: &str,
        action_tx: &mpsc::Sender<ActionMessage>,
    ) {
        let desired_state = match payload {
            "ON" => true,
            "OFF" => false,
            other => {
                warn!("Switch '{}' received unknown payload: {other}", self.name);
                return;
            }
        };

        info!("Switch '{}' → {payload}", self.name);
        let _action_guard = self.action_lock.lock().await;

        let success = match &self.action {
            SwitchAction::Exec(cmd) => {
                let state_arg = if desired_state { "on" } else { "off" };
                let full_cmd = format!("{cmd} {state_arg}");
                match execute_command(&full_cmd).await {
                    Ok(output) => {
                        info!("Switch command output: {output}");
                        true
                    }
                    Err(e) => {
                        error!("Switch '{}' command failed: {e}", self.name);
                        false
                    }
                }
            }
            SwitchAction::Dbus(dbus_config) => {
                match execute_dbus_switch(dbus_config, desired_state).await {
                    Ok(()) => true,
                    Err(e) => {
                        error!("Switch '{}' D-Bus call failed: {e}", self.name);
                        false
                    }
                }
            }
        };

        if success {
            *self.state.lock().await = desired_state;
        }
        self.publish_state(action_tx).await;
    }

    async fn on_resume(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        self.publish_state(action_tx).await;
    }
}

/// Execute a D-Bus method call for a switch action.
async fn execute_dbus_switch(
    config: &DbusActionConfig,
    state: bool,
) -> Result<(), crate::error::DbusError> {
    let conn = crate::dbus::client::session_connection().await?;

    let proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination(config.service.as_str())?
        .path(config.path.as_str())?
        .interface(config.interface.as_str())?
        .build()
        .await?;

    let _reply: () = proxy.call(config.method.as_str(), &(state,)).await?;
    info!("D-Bus switch call: {}({state})", config.method);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SwitchConfig;
    use crate::mqtt::discovery::ComponentType;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn test_switch() -> SwitchComponent {
        let config = SwitchConfig {
            name: "Night Light".to_string(),
            exec: Some("echo toggle".to_string()),
            dbus: None,
        };
        SwitchComponent::new(&config, "myhost")
    }

    #[test]
    fn switch_name_and_ids() {
        let sw = test_switch();
        assert_eq!(sw.name(), "Night Light");
        assert_eq!(sw.unique_id, "myhost_night_light_switch");
    }

    #[test]
    fn switch_topics() {
        let sw = test_switch();
        assert_eq!(
            sw.command_topic,
            "homeassistant/switch/myhost/night_light/set"
        );
        assert_eq!(
            sw.state_topic,
            "homeassistant/switch/myhost/night_light/state"
        );
    }

    #[test]
    fn switch_subscriptions() {
        let sw = test_switch();
        let subs = sw.subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0], "homeassistant/switch/myhost/night_light/set");
    }

    #[test]
    fn switch_discovery_component() {
        let sw = test_switch();
        let disc = sw.discovery_component();
        assert_eq!(disc.name, "Night Light");
        match disc.component_type {
            ComponentType::Switch {
                command_topic,
                state_topic,
            } => {
                assert_eq!(command_topic, "homeassistant/switch/myhost/night_light/set");
                assert_eq!(state_topic, "homeassistant/switch/myhost/night_light/state");
            }
            _ => panic!("Expected Switch component type"),
        }
    }

    #[test]
    fn switch_no_polling() {
        let sw = Arc::new(test_switch());
        let (tx, _rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        assert!(sw.spawn_polling(tx, shutdown).is_none());
    }

    #[tokio::test]
    async fn switch_initial_state_is_off() {
        let sw = test_switch();
        let state = *sw.state.lock().await;
        assert!(!state, "Switch should start in OFF state");
    }

    #[tokio::test]
    async fn switch_handle_on_updates_state() {
        let config = SwitchConfig {
            name: "Test".to_string(),
            exec: Some("true".to_string()), // always succeeds
            dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "ON", &tx).await;

        // State should now be ON
        assert!(*sw.state.lock().await);

        // Should have published state
        let message = rx.try_recv().expect("should have published state");
        assert!(message.topic().contains("state"));
        assert_eq!(message.payload(), "ON");
    }

    #[tokio::test]
    async fn switch_handle_off_updates_state() {
        let config = SwitchConfig {
            name: "Test".to_string(),
            exec: Some("true".to_string()),
            dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        // First turn ON
        sw.handle_message("topic", "ON", &tx).await;
        let _ = rx.try_recv(); // consume ON state publish

        // Then turn OFF
        sw.handle_message("topic", "OFF", &tx).await;
        assert!(!*sw.state.lock().await);

        let message = rx.try_recv().expect("should have published state");
        assert_eq!(message.payload(), "OFF");
    }

    #[tokio::test]
    async fn switch_unknown_payload_ignored() {
        let sw = test_switch();
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "TOGGLE", &tx).await;

        // State should remain OFF, no message published
        assert!(!*sw.state.lock().await);
        assert!(
            rx.try_recv().is_err(),
            "No state should be published for unknown payload"
        );
    }

    #[tokio::test]
    async fn switch_on_resume_publishes_current_state() {
        let sw = test_switch();
        let (tx, mut rx) = mpsc::channel(16);

        // Default state is OFF
        sw.on_resume(&tx).await;

        let message = rx.try_recv().expect("should have published on resume");
        assert!(message.topic().contains("state"));
        assert_eq!(message.payload(), "OFF");
    }

    #[tokio::test]
    async fn switch_failed_command_does_not_update_state() {
        let config = SwitchConfig {
            name: "Fail".to_string(),
            exec: Some("false".to_string()), // always fails (exit code 1)
            dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "ON", &tx).await;

        // State should remain OFF because command failed
        assert!(!*sw.state.lock().await);

        // But state should still be published (showing OFF)
        let message = rx.try_recv().expect("should still publish state");
        assert_eq!(message.payload(), "OFF");
    }
}
