// Suspend inhibitor functionality - internal utilities for power management

use futures::StreamExt;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zbus::{Connection, Proxy, Result};

use super::power_management::PowerEvent;

// Constants for D-Bus service names and paths
const DBUS_SERVICE_NAME: &str = "org.freedesktop.login1";
const DBUS_OBJECT_PATH: &str = "/org/freedesktop/login1";
const DBUS_INTERFACE_NAME: &str = "org.freedesktop.login1.Manager";
const APP_NAME: &str = "mqtt-agent";
const INHIBIT_MODE: &str = "delay";

/// Type of inhibitor to acquire from logind
#[derive(Debug, Clone, Copy)]
pub enum InhibitorType {
    /// Sleep inhibitor (suspend)
    Sleep,
    /// Shutdown inhibitor
    Shutdown,
}

impl InhibitorType {
    /// Convert the enum to the string value expected by D-Bus
    fn as_str(&self) -> &'static str {
        match self {
            InhibitorType::Sleep => "sleep",
            InhibitorType::Shutdown => "shutdown",
        }
    }
}

/// Represents an active inhibitor lock for a system power operation
pub struct Inhibitor {
    /// The file descriptor that represents the inhibitor lock
    /// Prefixed with underscore because it's not directly used,
    /// but must be kept alive to maintain the inhibitor
    _fd: zbus::zvariant::OwnedFd,

    /// The type of inhibitor that was created
    inhibitor_type: &'static str,
}

impl Inhibitor {
    /// Creates a new inhibitor of the specified type
    async fn new(connection: &Connection, what: InhibitorType, reason: &str) -> Result<Self> {
        let proxy = Proxy::new(
            connection,
            DBUS_SERVICE_NAME,
            DBUS_OBJECT_PATH,
            DBUS_INTERFACE_NAME,
        )
        .await?;

        // Call Inhibit method to get a file descriptor
        let what_str = what.as_str();
        let reply = proxy
            .call_method("Inhibit", &(what_str, APP_NAME, reason, INHIBIT_MODE))
            .await?;

        // Extract the file descriptor from the reply
        let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;

        debug!(
            "Acquired {} inhibitor lock with reason: {}",
            what_str, reason
        );
        Ok(Self {
            _fd: fd,
            inhibitor_type: what_str,
        })
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        debug!("Released {} inhibitor lock", self.inhibitor_type);
    }
}

/// Handles system power management events and inhibitor locks
///
/// This struct is responsible for:
/// - Connecting to the system D-Bus
/// - Creating and managing inhibitor locks
/// - Monitoring for system power events
/// - Broadcasting power events to interested components
pub struct PowerManager {
    /// Sender for broadcasting power events
    event_sender: broadcast::Sender<PowerEvent>,

    /// Receiver for power events - primarily used by the struct itself
    event_receiver: broadcast::Receiver<PowerEvent>,

    /// D-Bus connection used for all D-Bus operations
    connection: Option<Connection>,

    /// Active suspend inhibitor lock, if one has been created
    suspend_inhibitor: Option<Inhibitor>,

    /// Active shutdown inhibitor lock, if one has been created
    shutdown_inhibitor: Option<Inhibitor>,
}

impl PowerManager {
    /// Creates a new PowerManager with a default broadcast channel
    ///
    /// The channel size is set to 16 events, which should be sufficient for
    /// most use cases as events are typically processed quickly.
    pub(crate) fn new() -> Self {
        let (event_sender, event_receiver) = broadcast::channel(16);
        Self {
            event_sender,
            event_receiver,
            connection: None,
            suspend_inhibitor: None,
            shutdown_inhibitor: None,
        }
    }

    /// Creates a new PowerManager with a provided event sender
    ///
    /// This is useful when sharing the same event bus across multiple components.
    pub(crate) fn new_with_sender(sender: broadcast::Sender<PowerEvent>) -> Self {
        Self {
            event_sender: sender.clone(),
            event_receiver: sender.subscribe(),
            connection: None,
            suspend_inhibitor: None,
            shutdown_inhibitor: None,
        }
    }

    /// Connect to the system D-Bus
    ///
    /// This must be called before creating inhibitors or starting the monitor.
    pub async fn connect_dbus(&mut self) -> Result<()> {
        // Use the ensure_connection helper
        self.ensure_connection().await.map(|_| ())
    }

    /// Helper to ensure a D-Bus connection exists or create one
    ///
    /// Returns a reference to the connection if successful
    async fn ensure_connection(&mut self) -> Result<&Connection> {
        if self.connection.is_none() {
            // Try to connect to the system D-Bus
            let conn = Connection::system()
                .await
                .map_err(|e| zbus::Error::Failure(format!("Failed to connect to D-Bus: {}", e)))?;

            info!("Successfully connected to system D-Bus");
            self.connection = Some(conn);
        }

        Ok(self.connection.as_ref().unwrap())
    }

    /// Helper method to handle D-Bus errors in a consistent way
    ///
    /// For non-critical errors where we want to keep the task alive indefinitely
    async fn handle_dbus_error<T>(
        &self,
        error: impl std::fmt::Display,
        context: &str,
    ) -> Result<T> {
        warn!(
            "Failed to {}: {}. Power monitoring will be disabled.",
            context, error
        );

        // Sleep indefinitely to keep the task alive
        tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;

        // Return an Ok value since we're handling the error by sleeping
        Err(zbus::Error::Failure(format!(
            "Failed to {}: {}",
            context, error
        )))
    }

    /// Helper to create and store an inhibitor.
    async fn _create_and_store_inhibitor(
        &mut self,
        inhibitor_type: InhibitorType,
        reason: &str,
    ) -> Result<()> {
        let connection = self.ensure_connection().await?;

        let inhibitor = Inhibitor::new(connection, inhibitor_type, reason).await?;

        match inhibitor_type {
            InhibitorType::Sleep => self.suspend_inhibitor = Some(inhibitor),
            InhibitorType::Shutdown => self.shutdown_inhibitor = Some(inhibitor),
        }
        Ok(())
    }

    /// Create a suspend inhibitor with the given reason
    pub async fn create_suspend_inhibitor(&mut self, reason: &str) -> Result<()> {
        self._create_and_store_inhibitor(InhibitorType::Sleep, reason)
            .await
    }

    /// Create a shutdown inhibitor with the given reason
    pub async fn create_shutdown_inhibitor(&mut self, reason: &str) -> Result<()> {
        self._create_and_store_inhibitor(InhibitorType::Shutdown, reason)
            .await
    }

    /// Release the suspend inhibitor if one exists.
    /// The Drop implementation of Inhibitor will log its release.
    pub fn release_suspend_inhibitor(&mut self) {
        self.suspend_inhibitor.take();
    }

    /// Release the shutdown inhibitor if one exists.
    /// The Drop implementation of Inhibitor will log its release.
    pub fn release_shutdown_inhibitor(&mut self) {
        self.shutdown_inhibitor.take();
    }

    /// Run the power event monitor
    ///
    /// This method sets up a listener for power events and broadcasts them.
    /// It runs indefinitely and should be called in a separate task.
    pub(crate) async fn run_monitor(&mut self) -> Result<()> {
        // Use the ensure_connection helper to get or establish a connection
        let connection = match self.ensure_connection().await {
            Ok(conn) => conn.clone(),
            Err(e) => {
                return self.handle_dbus_error(e, "connect to system D-Bus").await;
            }
        };

        // Create a proxy for the login1 manager interface
        let proxy = match Proxy::new(
            &connection,
            DBUS_SERVICE_NAME,
            DBUS_OBJECT_PATH,
            DBUS_INTERFACE_NAME,
        )
        .await
        {
            Ok(p) => {
                debug!("Successfully created login1 manager proxy");
                p
            }
            Err(e) => {
                return self
                    .handle_dbus_error(e, "create login1 manager proxy")
                    .await;
            }
        };

        // Subscribe to the PrepareForSleep signal
        let sender = self.event_sender.clone();
        let mut stream = match proxy.receive_signal("PrepareForSleep").await {
            Ok(s) => {
                debug!("Successfully subscribed to PrepareForSleep signals");
                s
            }
            Err(e) => {
                return self
                    .handle_dbus_error(e, "subscribe to PrepareForSleep signals")
                    .await;
            }
        };

        info!("Power monitor started, listening for suspend/resume events");

        while let Some(msg) = stream.next().await {
            // Extract the boolean value from the signal and send appropriate event
            match msg.body().deserialize::<bool>() {
                Ok(true) => {
                    info!("System is about to suspend");
                    if let Err(e) = sender.send(PowerEvent::Suspending) {
                        error!("Failed to broadcast suspending event: {}", e);
                    }
                }
                Ok(false) => {
                    info!("System is resuming from suspend");
                    if let Err(e) = sender.send(PowerEvent::Resuming) {
                        error!("Failed to broadcast resuming event: {}", e);
                    }
                }
                Err(e) => error!("Failed to parse PrepareForSleep signal: {}", e),
            }
        }

        Ok(())
    }

    /// Get a clone of the event sender
    ///
    /// This can be used to create additional event receivers elsewhere in the application.
    pub(crate) fn clone_sender(&self) -> broadcast::Sender<PowerEvent> {
        self.event_sender.clone()
    }

    /// Gets a mutable reference to the power event receiver
    ///
    /// This method is used to receive power events from the monitoring task
    /// in the main application loop.
    ///
    /// # Returns
    /// * `&mut broadcast::Receiver<PowerEvent>` - Mutable reference to the event receiver
    ///
    /// # Examples
    /// ```
    /// let receiver = power_manager.get_receiver();
    /// ```
    pub fn get_receiver(&mut self) -> &mut broadcast::Receiver<PowerEvent> {
        &mut self.event_receiver
    }
}
