use tracing::info;
use zbus::Connection;

fn notification_icon(urgency: u8) -> &'static str {
    match urgency {
        2 => "dialog-warning",
        _ => "dialog-information",
    }
}

fn expire_timeout_ms(urgency: u8) -> i32 {
    match urgency {
        0 => 5_000,
        1 => 10_000,
        2 => 0,
        _ => 10_000,
    }
}

/// Send a desktop notification via D-Bus (`org.freedesktop.Notifications`).
pub async fn send_notification(
    session_conn: &Connection,
    summary: &str,
    body: &str,
    urgency: u8,
) -> Result<(), crate::error::DbusError> {
    let proxy: zbus::Proxy = zbus::proxy::Builder::new(session_conn)
        .destination("org.freedesktop.Notifications")?
        .path("/org/freedesktop/Notifications")?
        .interface("org.freedesktop.Notifications")?
        .build()
        .await?;

    // Build hints dictionary with urgency.
    let mut hints = std::collections::HashMap::new();
    hints.insert("urgency", zbus::zvariant::Value::from(urgency));

    let _reply: u32 = proxy
        .call(
            "Notify",
            &(
                "hars-imp",                 // app_name
                0u32,                       // replaces_id
                notification_icon(urgency), // app_icon
                summary,                    // summary
                body,                       // body
                Vec::<String>::new(),       // actions
                hints,                      // hints
                expire_timeout_ms(urgency), // expire_timeout
            ),
        )
        .await?;

    info!("Sent desktop notification: {summary}");
    Ok(())
}
