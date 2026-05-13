use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::components::trait_def::{ActionMessage, Component};
use crate::dbus::notifications as dbus_notify;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};

/// Incoming notification payload from HA.
#[derive(Debug, Deserialize)]
struct NotificationPayload {
    /// Notification title/summary.
    #[serde(alias = "title")]
    summary: String,

    /// Notification body text.
    message: String,

    /// Urgency level: "low", "normal", "critical".
    #[serde(default = "default_importance")]
    importance: String,
}

fn default_importance() -> String {
    "normal".to_string()
}

/// A notification entity that forwards HA notifications to the Linux desktop.
pub struct NotificationComponent {
    name: String,
    unique_id: String,
    command_topic: String,
}

impl NotificationComponent {
    pub fn new(hostname: &str) -> Self {
        Self {
            name: "Notification".to_string(),
            unique_id: format!("{hostname}_notification"),
            command_topic: format!("homeassistant/notify/{hostname}"),
        }
    }
}

#[async_trait]
impl Component for NotificationComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.name.clone(),
            unique_id: self.unique_id.clone(),
            component_type: ComponentType::Notify {
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
        let notification: NotificationPayload = match serde_json::from_str(payload) {
            Ok(n) => n,
            Err(e) => {
                warn!("Failed to parse notification JSON: {e}. Sending raw payload as fallback.");
                NotificationPayload {
                    summary: "Home Assistant".to_string(),
                    message: payload.to_string(),
                    importance: default_importance(),
                }
            }
        };

        let urgency = match notification.importance.as_str() {
            "low" => 0u8,
            "critical" => 2u8,
            _ => 1u8, // normal
        };

        // Try to send the desktop notification via session D-Bus.
        match crate::dbus::client::session_connection().await {
            Ok(conn) => {
                if let Err(e) = dbus_notify::send_notification(
                    conn,
                    &notification.summary,
                    &notification.message,
                    urgency,
                )
                .await
                {
                    error!("Failed to send desktop notification: {e}");
                }
            }
            Err(e) => {
                error!("Failed to connect to session D-Bus for notification: {e}");
            }
        }

        info!(
            "Notification: {} - {}",
            notification.summary, notification.message
        );
    }
}
