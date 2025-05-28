// Suspend inhibitor functionality - internal utilities for power management

use tokio::sync::broadcast;
use tracing::{error, info, warn, debug};
use zbus::{Connection, Result, Proxy};
use futures::StreamExt;
use std::os::unix::io::{OwnedFd, FromRawFd, AsRawFd};

use super::power_management::PowerEvent;

pub struct SuspendInhibitor {
    _fd: OwnedFd,
}

impl SuspendInhibitor {
    pub async fn new(connection: &Connection, reason: &str) -> Result<Self> {
        let proxy = Proxy::new(
            connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        ).await?;

        // Call Inhibit method to get a file descriptor
        let reply = proxy
            .call_method(
                "Inhibit",
                &("sleep", "mqtt-agent", reason, "delay"),
            )
            .await?;

        // Extract the file descriptor from the reply
        let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
        let owned_fd = unsafe { OwnedFd::from_raw_fd(fd.as_raw_fd()) };

        debug!("Acquired suspend inhibitor lock with reason: {}", reason);
        Ok(Self { _fd: owned_fd })
    }
}

impl Drop for SuspendInhibitor {
    fn drop(&mut self) {
        debug!("Released suspend inhibitor lock");
    }
}

pub struct PowerManager {
    event_sender: broadcast::Sender<PowerEvent>,
    event_receiver: broadcast::Receiver<PowerEvent>,
    connection: Option<Connection>,
    suspend_inhibitor: Option<SuspendInhibitor>,
}

impl PowerManager {
    pub(crate) fn new() -> Self {
        let (event_sender, event_receiver) = broadcast::channel(16);
        Self { 
            event_sender,
            event_receiver,
            connection: None,
            suspend_inhibitor: None,
        }
    }

    pub(crate) fn new_with_sender(sender: broadcast::Sender<PowerEvent>) -> Self {
        let event_receiver = sender.subscribe();
        Self {
            event_sender: sender,
            event_receiver,
            connection: None,
            suspend_inhibitor: None,
        }
    }

    pub async fn create_inhibitor(&mut self, reason: &str) -> Result<()> {
        if let Some(connection) = &self.connection {
            let inhibitor = SuspendInhibitor::new(connection, reason).await?;
            self.suspend_inhibitor = Some(inhibitor);
            Ok(())
        } else {
            Err(zbus::Error::Failure("No D-Bus connection available".to_string()))
        }
    }
    
    pub fn release_inhibitor(&mut self) {
        if self.suspend_inhibitor.take().is_some() {
            debug!("Released suspend inhibitor lock");
        }
    }

    pub(crate) async fn run_monitor(&mut self) -> Result<()> {
        // Try to connect to the system D-Bus
        let connection = match Connection::system().await {
            Ok(conn) => {
                info!("Successfully connected to system D-Bus");
                self.connection = Some(conn.clone());
                conn
            }
            Err(e) => {
                warn!("Failed to connect to system D-Bus: {}. Power monitoring will be disabled.", e);
                tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;
                return Ok(());
            }
        };
        
        // Create initial suspend inhibitor
        if let Err(e) = self.create_inhibitor("MQTT daemon startup - preventing unexpected suspension").await {
            warn!("Failed to create initial suspend inhibitor: {}", e);
        } else {
            info!("Created initial suspend inhibitor (delay mode with system default timeout)");
        }
        
        // Create a proxy for the login1 manager interface
        let proxy = match Proxy::new(
            &connection,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        ).await {
            Ok(p) => {
                info!("Successfully created login1 manager proxy");
                p
            }
            Err(e) => {
                warn!("Failed to create login1 manager proxy: {}. Power monitoring will be disabled.", e);
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
                info!("Successfully subscribed to PrepareForSleep signals");
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

    pub fn get_receiver(&mut self) -> &mut broadcast::Receiver<PowerEvent> {
        &mut self.event_receiver
    }
}
