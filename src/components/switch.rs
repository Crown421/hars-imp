use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::components::trait_def::{ActionMessage, Component, OutboundMessage};
use crate::config::{DbusActionConfig, SwitchConfig};
use crate::error::ComponentError;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};
use crate::util::helpers::{execute_command, slugify};

/// The action a switch performs: either a shell command or a D-Bus method call.
#[derive(Debug, Clone)]
enum SwitchAction {
    Exec(String),
    Dbus(DbusActionConfig),
}

/// Optional readback backend used to fetch the real current switch state.
#[derive(Debug, Clone)]
enum SwitchStatusReader {
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
    status_reader: Option<SwitchStatusReader>,

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
        let status_reader = config
            .status_exec
            .as_ref()
            .map(|exec| SwitchStatusReader::Exec(exec.clone()))
            .or_else(|| {
                config
                    .status_dbus
                    .as_ref()
                    .map(|dbus| SwitchStatusReader::Dbus(dbus.clone()))
            });

        Self {
            name: config.name.clone(),
            unique_id: format!("{hostname}_{slug}_switch"),
            command_topic: format!("homeassistant/switch/{hostname}/{slug}/set"),
            state_topic: format!("homeassistant/switch/{hostname}/{slug}/state"),
            action,
            status_reader,
            state: Mutex::new(false),
            action_lock: Mutex::new(()),
        }
    }

    /// Publish the current state to the state topic.
    async fn publish_state(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        let state = *self.state.lock().await;
        self.publish_known_state(action_tx, state).await;
    }

    async fn publish_known_state(&self, action_tx: &mpsc::Sender<ActionMessage>, state: bool) {
        let payload = if state { "ON" } else { "OFF" };
        let _ = action_tx
            .send(OutboundMessage::state(self.state_topic.clone(), payload))
            .await;
    }

    async fn read_state(&self) -> Result<bool, ComponentError> {
        let Some(status_reader) = &self.status_reader else {
            return Err(ComponentError::CommandFailed(
                "switch status readback is not configured".into(),
            ));
        };

        match status_reader {
            SwitchStatusReader::Exec(cmd) => {
                parse_switch_state_output(&execute_command(cmd).await?)
            }
            SwitchStatusReader::Dbus(config) => read_dbus_switch_state(config).await,
        }
    }

    async fn refresh_state_from_readback(
        &self,
        action_tx: &mpsc::Sender<ActionMessage>,
    ) -> Result<bool, ComponentError> {
        let state = self.read_state().await?;
        *self.state.lock().await = state;
        self.publish_known_state(action_tx, state).await;
        Ok(state)
    }

    async fn sync_and_publish_current_state(
        &self,
        action_tx: &mpsc::Sender<ActionMessage>,
        publish_cached_on_readback_failure: bool,
    ) -> Result<bool, ComponentError> {
        if self.status_reader.is_none() {
            let state = *self.state.lock().await;
            self.publish_known_state(action_tx, state).await;
            return Ok(state);
        }

        match self.refresh_state_from_readback(action_tx).await {
            Ok(state) => Ok(state),
            Err(err) => {
                if publish_cached_on_readback_failure {
                    self.publish_state(action_tx).await;
                }
                Err(err)
            }
        }
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
            if self.status_reader.is_none() {
                *self.state.lock().await = desired_state;
            }

            if let Err(e) = self.sync_and_publish_current_state(action_tx, false).await {
                if self.status_reader.is_some() {
                    error!(
                        "Switch '{}' status readback failed after command: {e}",
                        self.name
                    );
                }
            }
        } else {
            self.publish_state(action_tx).await;
        }
    }

    async fn sync_state(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        let _action_guard = self.action_lock.lock().await;

        if let Err(e) = self.sync_and_publish_current_state(action_tx, false).await {
            if self.status_reader.is_some() {
                error!("Switch '{}' status readback failed: {e}", self.name);
            }
        }
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

async fn read_dbus_switch_state(config: &DbusActionConfig) -> Result<bool, ComponentError> {
    let conn = crate::dbus::client::session_connection()
        .await
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?;

    let proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
        .destination(config.service.as_str())
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?
        .path(config.path.as_str())
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?
        .interface(config.interface.as_str())
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?
        .build()
        .await
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?;

    let reply = proxy
        .call_method(config.method.as_str(), &())
        .await
        .map_err(|e| ComponentError::CommandFailed(e.to_string()))?;
    let body = reply.body();

    if let Ok(state) = body.deserialize::<bool>() {
        return Ok(state);
    }

    if let Ok(state) = body.deserialize::<String>() {
        return parse_switch_state_output(&state);
    }

    Err(ComponentError::InvalidPayload(format!(
        "unsupported D-Bus switch status reply from {}",
        config.method
    )))
}

fn parse_switch_state_output(output: &str) -> Result<bool, ComponentError> {
    let normalized = output.trim();
    if normalized.eq_ignore_ascii_case("on")
        || normalized.eq_ignore_ascii_case("true")
        || normalized == "1"
    {
        return Ok(true);
    }

    if normalized.eq_ignore_ascii_case("off")
        || normalized.eq_ignore_ascii_case("false")
        || normalized == "0"
    {
        return Ok(false);
    }

    Err(ComponentError::InvalidPayload(format!(
        "unsupported switch state output '{normalized}'"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SwitchConfig;
    use crate::mqtt::discovery::ComponentType;
    use std::sync::Arc;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio_util::sync::CancellationToken;

    fn test_switch() -> SwitchComponent {
        let config = SwitchConfig {
            name: "Night Light".to_string(),
            exec: Some("echo toggle".to_string()),
            dbus: None,
            status_exec: None,
            status_dbus: None,
        };
        SwitchComponent::new(&config, "myhost")
    }

    fn write_test_script(contents: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        fs::write(&path, contents).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        (dir, path.display().to_string())
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
            status_exec: None,
            status_dbus: None,
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
            status_exec: None,
            status_dbus: None,
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
    async fn switch_sync_state_publishes_current_state() {
        let sw = test_switch();
        let (tx, mut rx) = mpsc::channel(16);

        // Default state is OFF
        sw.sync_state(&tx).await;

        let message = rx.try_recv().expect("should have published during sync");
        assert!(message.topic().contains("state"));
        assert_eq!(message.payload(), "OFF");
    }

    #[tokio::test]
    async fn switch_sync_state_reads_back_on_state() {
        let config = SwitchConfig {
            name: "Readback On".to_string(),
            exec: Some("true".to_string()),
            dbus: None,
            status_exec: Some("printf ON".to_string()),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.sync_state(&tx).await;

        let message = rx.try_recv().expect("should have published during sync");
        assert_eq!(message.payload(), "ON");
        assert!(*sw.state.lock().await);
    }

    #[tokio::test]
    async fn switch_sync_state_reads_back_off_state() {
        let config = SwitchConfig {
            name: "Readback Off".to_string(),
            exec: Some("true".to_string()),
            dbus: None,
            status_exec: Some("printf OFF".to_string()),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.sync_state(&tx).await;

        let message = rx.try_recv().expect("should have published during sync");
        assert_eq!(message.payload(), "OFF");
        assert!(!*sw.state.lock().await);
    }

    #[tokio::test]
    async fn switch_sync_state_skips_publish_when_readback_fails() {
        let config = SwitchConfig {
            name: "Readback Fail".to_string(),
            exec: Some("true".to_string()),
            dbus: None,
            status_exec: Some("printf maybe".to_string()),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.sync_state(&tx).await;

        assert!(
            rx.try_recv().is_err(),
            "readback failure should not publish"
        );
        assert!(!*sw.state.lock().await);
    }

    #[tokio::test]
    async fn switch_handle_message_publishes_readback_state() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("state.txt");
        fs::write(&state_file, "OFF").unwrap();

        let (_script_dir, script_path) = write_test_script(&format!(
            "#!/bin/sh\nif [ \"$1\" = \"on\" ]; then\n  printf ON > \"{}\"\nelse\n  printf OFF > \"{}\"\nfi\n",
            state_file.display(),
            state_file.display()
        ));
        let config = SwitchConfig {
            name: "Readback Switch".to_string(),
            exec: Some(script_path),
            dbus: None,
            status_exec: Some(format!("cat {}", state_file.display())),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "ON", &tx).await;

        let message = rx.try_recv().expect("should have published state");
        assert_eq!(message.payload(), "ON");
        assert!(*sw.state.lock().await);
    }

    #[tokio::test]
    async fn switch_sync_state_waits_for_in_flight_command_before_readback() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("state.txt");
        fs::write(&state_file, "OFF").unwrap();

        let (_script_dir, script_path) = write_test_script(&format!(
            "#!/bin/sh\nsleep 0.2\nif [ \"$1\" = \"on\" ]; then\n  printf ON > \"{}\"\nelse\n  printf OFF > \"{}\"\nfi\n",
            state_file.display(),
            state_file.display()
        ));
        let switch = Arc::new(SwitchComponent::new(
            &SwitchConfig {
                name: "Concurrent Switch".to_string(),
                exec: Some(script_path),
                dbus: None,
                status_exec: Some(format!("cat {}", state_file.display())),
                status_dbus: None,
            },
            "myhost",
        ));
        let (tx, mut rx) = mpsc::channel(16);

        let command_switch = Arc::clone(&switch);
        let command_tx = tx.clone();
        let command_task = tokio::spawn(async move {
            command_switch
                .handle_message("topic", "ON", &command_tx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sync_switch = Arc::clone(&switch);
        let sync_tx = tx.clone();
        let sync_task = tokio::spawn(async move {
            sync_switch.sync_state(&sync_tx).await;
        });

        command_task.await.unwrap();
        sync_task.await.unwrap();

        let first = rx.try_recv().expect("command should publish state");
        let second = rx.try_recv().expect("sync should publish state");
        assert_eq!(first.payload(), "ON");
        assert_eq!(second.payload(), "ON");
        assert!(rx.try_recv().is_err(), "should only publish twice");
        assert_eq!(fs::read_to_string(&state_file).unwrap(), "ON");
        assert!(*switch.state.lock().await);
    }

    #[tokio::test]
    async fn switch_failed_readback_after_command_publishes_nothing() {
        let config = SwitchConfig {
            name: "Broken Readback".to_string(),
            exec: Some("true".to_string()),
            dbus: None,
            status_exec: Some("printf maybe".to_string()),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "ON", &tx).await;

        assert!(rx.try_recv().is_err(), "failed readback should not publish");
        assert!(!*sw.state.lock().await);
    }

    #[tokio::test]
    async fn switch_failed_command_does_not_update_state() {
        let config = SwitchConfig {
            name: "Fail".to_string(),
            exec: Some("false".to_string()), // always fails (exit code 1)
            dbus: None,
            status_exec: None,
            status_dbus: None,
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

    #[tokio::test]
    async fn switch_failed_command_with_readback_keeps_cached_state() {
        let config = SwitchConfig {
            name: "Fail With Readback".to_string(),
            exec: Some("false".to_string()),
            dbus: None,
            status_exec: Some("printf ON".to_string()),
            status_dbus: None,
        };
        let sw = SwitchComponent::new(&config, "myhost");
        let (tx, mut rx) = mpsc::channel(16);

        sw.handle_message("topic", "ON", &tx).await;

        let message = rx.try_recv().expect("should still publish cached state");
        assert_eq!(message.payload(), "OFF");
        assert!(!*sw.state.lock().await);
    }
}
