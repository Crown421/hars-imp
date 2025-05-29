// Suspend inhibitor functionality - internal utilities for power management

use futures::StreamExt;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zbus::{Connection, Proxy, Result};

use super::power_management::PowerEvent;

pub struct Inhibitor {
    _fd: zbus::zvariant::OwnedFd,
    inhibitor_type: String,
}

impl Inhibitor {
    pub async fn new_suspend(connection: &Connection, reason: &str) -> Result<Self> {
        Self::new(connection, "sleep", reason, "delay").await
    }

    pub async fn new_shutdown(connection: &Connection, reason: &str) -> Result<Self> {
        Self::new(connection, "shutdown", reason, "delay").await
    }

    async fn new(connection: &Connection, what: &str, reason: &str, mode: &str) -> Result<Self> {
        let proxy = Proxy::new(
            connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await?;

        // Call Inhibit method to get a file descriptor
        let reply = proxy
            .call_method("Inhibit", &(what, "mqtt-agent", reason, mode))
            .await?;

        // Extract the file descriptor from the reply
        let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;

        debug!("Acquired {} inhibitor lock with reason: {}", what, reason);
        Ok(Self {
            _fd: fd,
            inhibitor_type: what.to_string(),
        })
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        debug!("Released {} inhibitor lock", self.inhibitor_type);
    }
}

pub struct PowerManager {
    event_sender: broadcast::Sender<PowerEvent>,
    event_receiver: broadcast::Receiver<PowerEvent>,
    connection: Option<Connection>,
    suspend_inhibitor: Option<Inhibitor>,
    shutdown_inhibitor: Option<Inhibitor>,
}

impl PowerManager {
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

    pub(crate) fn new_with_sender(sender: broadcast::Sender<PowerEvent>) -> Self {
        let event_receiver = sender.subscribe();
        Self {
            event_sender: sender,
            event_receiver,
            connection: None,
            suspend_inhibitor: None,
            shutdown_inhibitor: None,
        }
    }

    pub async fn connect_dbus(&mut self) -> Result<()> {
        // Try to connect to the system D-Bus
        match Connection::system().await {
            Ok(conn) => {
                info!("Successfully connected to system D-Bus");
                self.connection = Some(conn);
                Ok(())
            }
            Err(e) => Err(zbus::Error::Failure(format!(
                "Failed to connect to D-Bus: {}",
                e
            ))),
        }
    }

    pub async fn create_suspend_inhibitor(&mut self, reason: &str) -> Result<()> {
        if let Some(connection) = &self.connection {
            let inhibitor = Inhibitor::new_suspend(connection, reason).await?;
            self.suspend_inhibitor = Some(inhibitor);
            Ok(())
        } else {
            Err(zbus::Error::Failure(
                "No D-Bus connection available".to_string(),
            ))
        }
    }

    pub async fn create_shutdown_inhibitor(&mut self, reason: &str) -> Result<()> {
        if let Some(connection) = &self.connection {
            let inhibitor = Inhibitor::new_shutdown(connection, reason).await?;
            self.shutdown_inhibitor = Some(inhibitor);
            Ok(())
        } else {
            Err(zbus::Error::Failure(
                "No D-Bus connection available".to_string(),
            ))
        }
    }

    pub fn release_suspend_inhibitor(&mut self) {
        if self.suspend_inhibitor.take().is_some() {
            debug!("Released suspend inhibitor lock");
        }
    }

    pub fn release_shutdown_inhibitor(&mut self) {
        if self.shutdown_inhibitor.take().is_some() {
            debug!("Released shutdown inhibitor lock");
        }
    }

    pub(crate) async fn run_monitor(&mut self) -> Result<()> {
        // Use the existing connection if available, otherwise try to connect
        let connection = if let Some(conn) = &self.connection {
            conn.clone()
        } else {
            match Connection::system().await {
                Ok(conn) => {
                    debug!("Successfully connected to system D-Bus for monitoring");
                    self.connection = Some(conn.clone());
                    conn
                }
                Err(e) => {
                    warn!(
                        "Failed to connect to system D-Bus: {}. Power monitoring will be disabled.",
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;
                    return Ok(());
                }
            }
        };

        // Create a proxy for the login1 manager interface
        let proxy = match Proxy::new(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        {
            Ok(p) => {
                debug!("Successfully created login1 manager proxy");
                p
            }
            Err(e) => {
                warn!(
                    "Failed to create login1 manager proxy: {}. Power monitoring will be disabled.",
                    e
                );
                warn!("This may happen if systemd-logind is not running.");
                // Keep the monitor "running" but just wait indefinitely
                tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;
                return Ok(());
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
                warn!("Failed to subscribe to PrepareForSleep signals: {}. Power monitoring will be disabled.", e);
                // Keep the monitor "running" but just wait indefinitely
                tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;
                return Ok(());
            }
        };

        info!("Power monitor started, listening for suspend/resume events");

        while let Some(msg) = stream.next().await {
            // Extract the boolean value from the signal
            match msg.body().deserialize::<bool>() {
                Ok(true) => {
                    info!("System is about to suspend");
                    let _ = sender.send(PowerEvent::Suspending);
                }
                Ok(false) => {
                    info!("System is resuming from suspend");
                    let _ = sender.send(PowerEvent::Resuming);
                }
                Err(e) => error!("Failed to parse PrepareForSleep signal: {}", e),
            }
        }

        Ok(())
    }

    pub(crate) fn clone_sender(&self) -> broadcast::Sender<PowerEvent> {
        self.event_sender.clone()
    }

    /// Gets a mutable reference to the power event receiver.
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
