use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::components::trait_def::{ActionMessage, Component};
use crate::config::ButtonConfig;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};

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

/// Execute a shell command asynchronously and return stdout.
pub async fn execute_command(cmd: &str) -> Result<String, crate::error::ComponentError> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .await
        .map_err(|e| crate::error::ComponentError::CommandFailed(e.to_string()))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(crate::error::ComponentError::CommandFailed(stderr))
    }
}

/// Convert a name to a URL/topic-safe slug.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "_")
        .trim_matches('_')
        .to_string()
}
