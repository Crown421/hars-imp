use tokio::sync::broadcast;
use tracing::{error, info, warn, debug};
use zbus::{Connection, Result, Proxy};
use futures::StreamExt;
use std::os::unix::io::{OwnedFd, FromRawFd, AsRawFd};
use rumqttc::AsyncClient;

#[derive(Debug, Clone)]
pub enum PowerEvent {
    Suspending,
    Resuming,
}

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
    fn new() -> Self {
        let (event_sender, event_receiver) = broadcast::channel(16);
        Self { 
            event_sender,
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

    async fn run_monitor(&mut self) -> Result<()> {
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
}

/// Setup function to initialize power monitoring
/// Returns a PowerManager instance and starts the monitoring task
pub async fn setup_power_monitoring() -> (PowerManager, tokio::task::JoinHandle<()>) {
    let power_manager = PowerManager::new();
    
    // Create a separate instance for the background monitoring task
    let mut monitor_instance = PowerManager {
        event_sender: power_manager.event_sender.clone(),
        event_receiver: power_manager.event_sender.subscribe(),
        connection: None,
        suspend_inhibitor: None,
    };
    
    // Start power monitoring in background
    let monitor_handle = tokio::spawn(async move {
        if let Err(e) = monitor_instance.run_monitor().await {
            warn!("Power monitor encountered an error: {}", e);
            warn!("Power monitoring functionality will be unavailable.");
        }
    });
    
    (power_manager, monitor_handle)
}

/// Function to handle power events in the main tokio select loop
/// Returns Some(PowerEvent) if an event was received, None if channel is closed
pub async fn handle_power_events(
    power_manager: &mut PowerManager,
) -> Option<PowerEvent> {
    match power_manager.event_receiver.recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            debug!("Power event channel closed");
            None
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!("Power event receiver lagged, skipped {} events", skipped);
            // Try to receive the next event
            match power_manager.event_receiver.recv().await {
                Ok(event) => Some(event),
                Err(_) => None,
            }
        }
    }
}

/// Handle power event actions for suspending
pub async fn handle_suspend_actions(
    power_manager: &mut PowerManager,
    client: &mut AsyncClient,
    status_manager: &mut crate::status::StatusManager,
    system_monitor_handle: &mut tokio::task::JoinHandle<()>,
) {
    info!("System is about to suspend, performing shutdown actions...");
    
    // We already have an inhibitor from startup, so we can proceed with shutdown actions
    // The existing inhibitor gives us up to 2 seconds to complete our work
    
    // Perform critical shutdown actions
    if let Err(e) = status_manager.publish_suspended().await {
        error!("Failed to publish suspend status: {}", e);
    } else {
        debug!("Successfully published 'Suspended' status before suspend");
    }
    
    // Stop system monitoring
    system_monitor_handle.abort();
    debug!("Stopped system monitoring");
    
    // Gracefully disconnect MQTT client
    info!("Disconnecting MQTT client before suspend");
    client.disconnect().await.unwrap_or_else(|e| {
        warn!("Error during MQTT disconnect: {}", e);
    });
    debug!("MQTT client disconnected");
    
    // Release the inhibitor to allow the system to suspend
    power_manager.release_inhibitor();
    debug!("Pre-suspend actions completed, released inhibitor to allow system suspend");
}

/// Handle power event actions for resuming
pub async fn handle_resume_actions<F, Fut>(
    power_manager: &mut PowerManager,
    client: &mut AsyncClient,
    eventloop: &mut rumqttc::EventLoop,
    button_topics: &mut Vec<(String, String)>,
    status_manager: &mut crate::status::StatusManager,
    system_monitor_handle: &mut tokio::task::JoinHandle<()>,
    initialize_mqtt_fn: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(AsyncClient, rumqttc::EventLoop, Vec<(String, String)>, crate::status::StatusManager, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>>>,
{
    info!("System resumed from suspend, re-establishing connections...");
    
    // Recreate the suspend inhibitor for future suspension events
    if let Err(e) = power_manager.create_inhibitor("MQTT daemon running - preventing unexpected suspension").await {
        warn!("Failed to recreate suspend inhibitor after resume: {}", e);
    } else {
        debug!("Recreated suspend inhibitor after resume");
    }
    
    // Re-initialize MQTT connection
    info!("Re-initializing MQTT connection after resume");
    match initialize_mqtt_fn().await {
        Ok((new_client, new_eventloop, new_button_topics, new_status_manager, new_monitoring_handle)) => {
            *client = new_client;
            *eventloop = new_eventloop;
            *button_topics = new_button_topics;
            *status_manager = new_status_manager;
            *system_monitor_handle = new_monitoring_handle;
            
            info!("MQTT connection re-established successfully");
            debug!("Successfully published 'On' status after resume");
        }
        Err(e) => {
            error!("Failed to re-establish MQTT connection after resume: {}", e);
            // Continue with the old connection and hope it recovers
        }
    }
}


