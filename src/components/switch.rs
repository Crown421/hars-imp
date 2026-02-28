use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::components::trait_def::{ActionMessage, Component};
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
        }
    }

    /// Publish the current state to the state topic.
    async fn publish_state(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        let state = *self.state.lock().await;
        let payload = if state { "ON" } else { "OFF" };
        let _ = action_tx
            .send((self.state_topic.clone(), payload.to_string()))
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

    fn spawn_polling(
        self: Arc<Self>,
        _action_tx: mpsc::Sender<ActionMessage>,
        _shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        None // Switches are event-only.
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
