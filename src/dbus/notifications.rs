use tracing::{debug, error, info, warn};
use zbus::{Connection, Proxy};

/// Send a desktop notification via D-Bus
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
    
    // Create proxy for notifications service
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    
    // Notification parameters
    let app_name = "MQTT Agent";
    let replaces_id: u32 = 0;
    let app_icon = match urgency {
        0 => "dialog-information",        // Low urgency
        1 => "dialog-information",        // Normal urgency  
        2 => "dialog-warning",            // High/Critical urgency
        _ => "dialog-information",
    };
    let timeout: i32 = match urgency {
        0 => 5000,      // Low urgency: 5 seconds
        1 => 10000,     // Normal urgency: 10 seconds
        2 => 0,         // High/Critical urgency: persistent (0 = no timeout)
        _ => 10000,
    };
    
    // Create hints map with urgency
    let mut hints = std::collections::HashMap::new();
    hints.insert("urgency", zbus::zvariant::Value::U8(urgency));
    
    // Add category hint
    hints.insert("category", zbus::zvariant::Value::Str("im.received".into()));
    
    // Call the Notify method
    match proxy
        .call_method(
            "Notify",
            &(
                app_name,
                replaces_id,
                app_icon,
                summary,
                message,
                Vec::<String>::new(), // actions
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

/// Test if the notification service is available
pub async fn test_notification_service() -> bool {
    match Connection::session().await {
        Ok(connection) => {
            match Proxy::new(
                &connection,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await
            {
                Ok(proxy) => {
                    // Try to get server information to test if the service is available
                    match proxy.call_method("GetServerInformation", &()).await {
                        Ok(_) => {
                            debug!("Notification service is available");
                            true
                        }
                        Err(e) => {
                            warn!("Notification service not responding: {}", e);
                            false
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to create notification proxy: {}", e);
                    false
                }
            }
        }
        Err(e) => {
            warn!("No session D-Bus available for notifications: {}", e);
            false
        }
    }
}
