use tracing::info;
use zbus::Connection;

use crate::error::DbusError;

/// Create a new connection to the system D-Bus.
pub async fn system_connection() -> Result<Connection, DbusError> {
    info!("Connecting to system D-Bus");
    let conn = Connection::system().await?;
    Ok(conn)
}

/// Create a new connection to the session D-Bus.
#[allow(dead_code)]
pub async fn session_connection() -> Result<Connection, DbusError> {
    info!("Connecting to session D-Bus");
    let conn = Connection::session().await?;
    Ok(conn)
}
