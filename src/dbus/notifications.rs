use tracing::info;
use zbus::Connection;

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
                "hars-imp",    // app_name
                0u32,          // replaces_id
                "",            // app_icon
                summary,       // summary
                body,          // body
                Vec::<String>::new(), // actions
                hints,         // hints
                -1i32,         // expire_timeout (-1 = default)
            ),
        )
        .await?;

    info!("Sent desktop notification: {summary}");
    Ok(())
}
