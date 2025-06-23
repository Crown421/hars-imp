use std::collections::HashMap;
use tracing::{debug, error, info, warn};
use zbus::{Connection, zvariant::Value};

/// Send a desktop notification via D-Bus using low-level call_method
pub async fn send_desktop_notification(
    summary: &str,
    message: &str,
    urgency: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("Sending desktop notification: {} - {}", summary, message);

    // Try to connect to session D-Bus first
    let connection = match Connection::session().await {
        Ok(conn) => {
            debug!("Connected to session D-Bus for notifications");
            conn
        }
        Err(e) => {
            warn!("Failed to connect to session D-Bus: {}", e);
            // Fall back to system D-Bus if session is not available
            debug!("Attempting to connect to system D-Bus as fallback");
            Connection::system().await.map_err(|sys_err| {
                format!("Failed to connect to both session and system D-Bus. Session error: {}, System error: {}", e, sys_err)
            })?
        }
    };

    // Notification parameters
    let app_name = "MQTT Agent";
    let replaces_id: u32 = 0;
    let app_icon = match urgency {
        0 => "dialog-information", // Low urgency
        1 => "dialog-information", // Normal urgency
        2 => "dialog-warning",     // High/Critical urgency
        _ => "dialog-information",
    };
    let timeout: i32 = match urgency {
        0 => 5000,  // Low urgency: 5 seconds
        1 => 10000, // Normal urgency: 10 seconds
        2 => 0,     // High/Critical urgency: persistent (0 = no timeout)
        _ => 10000,
    };

    // Create hints map with urgency - use owned values to avoid lifetime issues
    let urgency_value = Value::U8(urgency);
    let category_value = Value::Str("im.received".into());
    let mut hints = HashMap::new();
    hints.insert("urgency", &urgency_value);
    hints.insert("category", &category_value);

    // Use low-level call_method directly on the connection
    match connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                app_name,
                replaces_id,
                app_icon,
                summary,
                message,
                vec![""; 0], // actions (empty array)
                hints,
                timeout,
            ),
        )
        .await
    {
        Ok(response) => {
            let notification_id: u32 = response.body().deserialize()?;
            info!(
                "Desktop notification sent successfully (ID: {}): {}",
                notification_id, summary
            );
            Ok(())
        }
        Err(e) => {
            error!("Failed to send desktop notification: {}", e);
            Err(e.into())
        }
    }
}
