use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::components::trait_def::{ActionMessage, Component};
use crate::config::ButtonConfig;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};
use crate::util::helpers::{execute_command, slugify};

/// A button entity that executes a shell command when pressed.
pub struct ButtonComponent {
    /// Config-provided name.
    name: String,

    /// Unique ID for HA entity registry.
    unique_id: String,

    /// MQTT topic to listen for press commands.
    command_topic: String,

    /// Shell command to execute.
    exec: String,
}

impl ButtonComponent {
    /// Create a button component from config.
    pub fn new(config: &ButtonConfig, hostname: &str) -> Self {
        let slug = slugify(&config.name);
        Self {
            name: config.name.clone(),
            unique_id: format!("{hostname}_{slug}_button"),
            command_topic: format!("homeassistant/button/{hostname}/{slug}/set"),
            exec: config.exec.clone(),
        }
    }
}

#[async_trait]
impl Component for ButtonComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.name.clone(),
            unique_id: self.unique_id.clone(),
            component_type: ComponentType::Button {
                command_topic: self.command_topic.clone(),
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
        _action_tx: &mpsc::Sender<ActionMessage>,
    ) {
        if payload == "PRESS" {
            info!("Button '{}' pressed, executing: {}", self.name, self.exec);
            match execute_command(&self.exec).await {
                Ok(output) => info!("Command output: {output}"),
                Err(e) => error!("Command failed for button '{}': {e}", self.name),
            }
        }
    }

    fn spawn_polling(
        self: Arc<Self>,
        _action_tx: mpsc::Sender<ActionMessage>,
        _shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        None // Buttons are event-only.
    }

    async fn on_resume(&self, _action_tx: &mpsc::Sender<ActionMessage>) {
        // Buttons have no state to re-publish.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ButtonConfig;
    use crate::mqtt::discovery::ComponentType;

    fn test_button() -> ButtonComponent {
        let config = ButtonConfig {
            name: "Lock Screen".to_string(),
            exec: "echo locked".to_string(),
        };
        ButtonComponent::new(&config, "myhost")
    }

    #[test]
    fn button_name_and_ids() {
        let btn = test_button();
        assert_eq!(btn.name(), "Lock Screen");
        assert_eq!(btn.unique_id, "myhost_lock_screen_button");
    }

    #[test]
    fn button_command_topic() {
        let btn = test_button();
        assert_eq!(btn.command_topic, "homeassistant/button/myhost/lock_screen/set");
    }

    #[test]
    fn button_subscriptions() {
        let btn = test_button();
        let subs = btn.subscriptions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0], "homeassistant/button/myhost/lock_screen/set");
    }

    #[test]
    fn button_discovery_component() {
        let btn = test_button();
        let disc = btn.discovery_component();
        assert_eq!(disc.name, "Lock Screen");
        assert_eq!(disc.unique_id, "myhost_lock_screen_button");
        match disc.component_type {
            ComponentType::Button { command_topic } => {
                assert_eq!(command_topic, "homeassistant/button/myhost/lock_screen/set");
            }
            _ => panic!("Expected Button component type"),
        }
    }

    #[test]
    fn button_no_polling() {
        let btn = Arc::new(test_button());
        let (tx, _rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        assert!(btn.spawn_polling(tx, shutdown).is_none());
    }

    #[tokio::test]
    async fn button_handle_press_executes_command() {
        let config = ButtonConfig {
            name: "Echo Test".to_string(),
            exec: "echo hello".to_string(),
        };
        let btn = ButtonComponent::new(&config, "myhost");
        let (tx, _rx) = mpsc::channel(16);

        // Should not panic — executes `echo hello`
        btn.handle_message("topic", "PRESS", &tx).await;
    }

    #[tokio::test]
    async fn button_ignores_non_press_payload() {
        let btn = test_button();
        let (tx, mut rx) = mpsc::channel(16);

        // Non-PRESS payloads should be silently ignored
        btn.handle_message("topic", "ON", &tx).await;
        btn.handle_message("topic", "", &tx).await;
        btn.handle_message("topic", "press", &tx).await; // case-sensitive

        // No action messages should have been sent
        assert!(rx.try_recv().is_err());
    }
}
