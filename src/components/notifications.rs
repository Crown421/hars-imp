use crate::ha_mqtt::HomeAssistantComponent;
use crate::utils::Config;
use rumqttc::{AsyncClient, QoS};
use serde::Deserialize;
use tracing::{debug, error, info, warn};

/// Notification payload structure expected from Home Assistant
#[derive(Deserialize, Debug)]
pub struct NotificationPayload {
    pub summary: String,
    pub message: String,
    pub importance: Option<String>, // low, normal, high, critical
}

impl NotificationPayload {
    /// Get the urgency level for D-Bus notifications
    pub fn get_urgency(&self) -> u8 {
        match self.importance.as_deref() {
            Some("low") => 0,           // Low urgency
            Some("normal") | None => 1, // Normal urgency (default)
            Some("high") => 2,          // Critical urgency
            _ => 1,                     // Default to normal for unknown values
        }
    }
}

/// Send a system notification via D-Bus
pub async fn send_system_notification(
    summary: &str,
    message: &str,
    urgency: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::dbus::send_desktop_notification;
    send_desktop_notification(summary, message, urgency).await
}

/// Handle notification command from MQTT
pub async fn handle_notification_command(
    topic: &str,
    payload: &str,
    notification_topic: &str,
) -> bool {
    if topic == notification_topic {
        debug!(
            "Received notification command on topic '{}': {}",
            topic, payload
        );

        // Try to parse JSON payload
        match serde_json::from_str::<NotificationPayload>(payload) {
            Ok(notification) => {
                info!(
                    "Processing notification: {} - {} (importance: {:?})",
                    notification.summary, notification.message, notification.importance
                );

                let urgency = notification.get_urgency();

                // Send the system notification
                match send_system_notification(
                    &notification.summary,
                    &notification.message,
                    urgency,
                )
                .await
                {
                    Ok(()) => {
                        info!("Notification sent successfully");
                    }
                    Err(e) => {
                        error!("Failed to send notification: {}", e);
                    }
                }
            }
            Err(e) => {
                error!(
                    "Failed to parse notification JSON on topic '{}': {}. Payload: {}",
                    topic, e, payload
                );

                // Try to send a fallback notification with the raw payload
                warn!("Sending fallback notification with raw payload");
                if let Err(e) = send_system_notification(
                    "MQTT Notification",
                    payload,
                    1, // Normal urgency
                )
                .await
                {
                    error!("Failed to send fallback notification: {}", e);
                }
            }
        }
        return true;
    }
    false
}

/// Creates a built-in notification component and returns the notification topic for subscription
pub async fn create_notification_components_and_setup(
    client: &AsyncClient,
    config: &Config,
) -> Result<(Vec<(String, HomeAssistantComponent)>, String), Box<dyn std::error::Error>> {
    let notification_id = format!("{}_notifications", config.hostname);
    let notification_topic = format!("homeassistant/notify/{}/command", notification_id);

    // Create the notification component
    let component = HomeAssistantComponent::notify(
        "Notifications".to_string(),
        notification_id.clone(),
        notification_topic.clone(),
    );

    // Subscribe to notification command topic
    debug!("Subscribing to notification topic: {}", notification_topic);
    client
        .subscribe(&notification_topic, QoS::AtMostOnce)
        .await?;

    let notification_components = vec![(notification_id, component)];

    Ok((notification_components, notification_topic))
}
