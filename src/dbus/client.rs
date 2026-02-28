use tokio::sync::OnceCell;
use tracing::info;
use zbus::Connection;

use crate::error::DbusError;

/// Lazily-initialized shared session D-Bus connection.
static SESSION_CONNECTION: OnceCell<Connection> = OnceCell::const_new();

/// Create a new connection to the system D-Bus.
pub async fn system_connection() -> Result<Connection, DbusError> {
    info!("Connecting to system D-Bus");
    let conn = Connection::system().await?;
    Ok(conn)
}

/// Get (or create) the shared session D-Bus connection.
///
/// The connection is created on first call and reused thereafter.
/// This avoids opening a new session bus connection for every
/// notification or D-Bus switch call.
pub async fn session_connection() -> Result<&'static Connection, DbusError> {
    SESSION_CONNECTION
        .get_or_try_init(|| async {
            info!("Connecting to session D-Bus (shared)");
            let conn = Connection::session().await?;
            Ok(conn)
        })
        .await
}
