// Power management module - handles power events and system state management

use rumqttc::AsyncClient;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::inhibitor::PowerManager;
use crate::Config;
use crate::dbus::status::StatusManager;
use crate::ha_mqtt::TopicHandlers;
use crate::shutdown::{ShutdownScenario, perform_graceful_mqtt_shutdown};

/// Power event types that can be received from the system
#[derive(Debug, Clone)]
pub enum PowerEvent {
    Suspending,
    Resuming,
}

/// Setup function to initialize power monitoring and create inhibitors
/// Returns a PowerManager instance and starts the monitoring task
pub async fn setup_power_monitoring() -> (PowerManager, tokio::task::JoinHandle<()>) {
    let mut power_manager = PowerManager::new();

    // Establish D-Bus connection once for both monitoring and inhibitors
    if let Err(e) = power_manager.connect_dbus().await {
        warn!("Failed to connect to D-Bus: {}", e);
        warn!("Power monitoring and inhibitors will be unavailable.");

        // Create a dummy monitoring task that just waits indefinitely
        let monitor_handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(u64::MAX)).await;
        });

        return (power_manager, monitor_handle);
    }

    // Create inhibitors using the established connection
    // Create suspend inhibitor
    if let Err(e) = power_manager
        .create_suspend_inhibitor("MQTT daemon startup - preventing unexpected suspension")
        .await
    {
        warn!("Failed to create suspend inhibitor: {}", e);
    } else {
        info!("Created suspend inhibitor (delay mode with system default timeout)");
    }

    // Create shutdown inhibitor
    if let Err(e) = power_manager
        .create_shutdown_inhibitor("MQTT daemon graceful shutdown - allowing cleanup time")
        .await
    {
        warn!("Failed to create shutdown inhibitor: {}", e);
    } else {
        info!("Created shutdown inhibitor (delay mode with system default timeout)");
    }

    // Get the sender for creating a new PowerManager for the main loop
    let event_sender = power_manager.clone_sender();

    // Start power monitoring using the same PowerManager instance
    let monitor_handle = tokio::spawn(async move {
        if let Err(e) = power_manager.run_monitor().await {
            warn!("Power monitor encountered an error: {}", e);
            warn!("Power monitoring functionality will be unavailable.");
        }
    });

    // Create a new PowerManager for the main loop (with shared sender)
    let main_power_manager = PowerManager::new_with_sender(event_sender);

    (main_power_manager, monitor_handle)
}

/// Function to handle power events in the main tokio select loop
/// Returns Some(PowerEvent) if an event was received, None if channel is closed
pub async fn handle_power_events(power_manager: &mut PowerManager) -> Option<PowerEvent> {
    match power_manager.get_receiver().recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            debug!("Power event channel closed");
            None
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!("Power event receiver lagged, skipped {} events", skipped);
            // Try to receive the next event without the nested match
            power_manager.get_receiver().recv().await.ok()
        }
    }
}
/// Handler for power events that encapsulates all power management actions
pub struct PowerEventHandler<'a> {
    power_manager: &'a mut PowerManager,
    client: &'a mut AsyncClient,
    eventloop: &'a mut rumqttc::EventLoop,
    topic_handlers: &'a mut TopicHandlers,
    status_manager: &'a mut StatusManager,
    system_monitor_handle: &'a mut tokio::task::JoinHandle<()>,
    config: &'a Config,
}

impl<'a> PowerEventHandler<'a> {
    /// Create a new power event handler with all required components
    pub fn new(
        power_manager: &'a mut PowerManager,
        client: &'a mut AsyncClient,
        eventloop: &'a mut rumqttc::EventLoop,
        topic_handlers: &'a mut TopicHandlers,
        status_manager: &'a mut StatusManager,
        system_monitor_handle: &'a mut tokio::task::JoinHandle<()>,
        config: &'a Config,
    ) -> Self {
        Self {
            power_manager,
            client,
            eventloop,
            topic_handlers,
            status_manager,
            system_monitor_handle,
            config,
        }
    }

    /// Handle a power event by dispatching to the appropriate handler method
    pub async fn handle_event(&mut self, event: PowerEvent) {
        match event {
            PowerEvent::Suspending => self.handle_suspend().await,
            PowerEvent::Resuming => self.handle_resume().await,
            // Add future power events here (e.g., Hibernating, PowerSaving)
        }
    }

    /// Handle system suspend by gracefully shutting down services
    async fn handle_suspend(&mut self) {
        info!("System is about to suspend, performing shutdown actions...");

        // We already have an inhibitor from startup, so we can proceed with shutdown actions
        // The existing inhibitor gives us up to 2 seconds to complete our work

        // Stop system monitoring
        self.system_monitor_handle.abort();
        debug!("Stopped system monitoring");

        // Use the general MQTT shutdown function with proper event queue draining
        if let Err(e) = perform_graceful_mqtt_shutdown(
            self.status_manager,
            self.client,
            self.eventloop,
            ShutdownScenario::Suspend,
        )
        .await
        {
            error!(
                "Failed to perform graceful MQTT shutdown for suspend: {}",
                e
            );
        }

        // Release the inhibitor to allow the system to suspend
        self.power_manager.release_suspend_inhibitor();
        debug!("Pre-suspend actions completed, released inhibitor to allow system suspend");
    }

    /// Helper method for retry logic with exponential backoff
    async fn retry_dbus_operation<T, E>(
        &mut self,
        operation_name: &str,
        operation: impl Fn(
            &mut PowerManager,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + '_>>,
        max_retries: u32,
    ) -> Result<T, E>
    where
        E: std::fmt::Display,
    {
        let mut attempt = 0;
        let mut delay_ms = 500; // Start with 500ms delay

        loop {
            attempt += 1;
            match operation(self.power_manager).await {
                Ok(result) => {
                    debug!(
                        "{} succeeded (attempt {}/{})",
                        operation_name, attempt, max_retries
                    );
                    return Ok(result);
                }
                Err(e) => {
                    if attempt >= max_retries {
                        warn!(
                            "Failed {} after {} attempts: {}",
                            operation_name, max_retries, e
                        );
                        return Err(e);
                    } else {
                        debug!(
                            "Attempt {}/{} for {} failed: {}. Retrying in {}ms",
                            attempt, max_retries, operation_name, e, delay_ms
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        delay_ms *= 2; // Exponential backoff
                    }
                }
            }
        }
    }

    /// Handle system resume by re-establishing connections and services
    async fn handle_resume(&mut self) {
        info!("System resumed from suspend, re-establishing connections...");

        // Re-initialize MQTT connection
        info!("Re-initializing MQTT connection after resume");
        match crate::ha_mqtt::initialize_mqtt_connection(self.config).await {
            Ok((
                new_client,
                new_eventloop,
                new_topic_handlers,
                new_status_manager,
                new_monitoring_handle,
            )) => {
                *self.client = new_client;
                *self.eventloop = new_eventloop;
                *self.topic_handlers = new_topic_handlers;
                *self.status_manager = new_status_manager;
                *self.system_monitor_handle = new_monitoring_handle;

                info!("MQTT connection re-established successfully");
                debug!("Successfully published 'On' status after resume");
            }
            Err(e) => {
                error!("Failed to re-establish MQTT connection after resume: {}", e);
                // Continue with the old connection and hope it recovers
            }
        }

        // Reconnect to D-Bus with retry
        let max_retries = 3;

        // Try to reconnect to D-Bus
        if let Err(e) = self
            .retry_dbus_operation(
                "D-Bus reconnection",
                |pm| Box::pin(pm.connect_dbus()),
                max_retries,
            )
            .await
        {
            warn!("Failed to reconnect to D-Bus: {}", e);
            return; // Don't try to create inhibitor if we can't connect to D-Bus
        }

        // Recreate suspend inhibitor
        if let Err(e) = self
            .retry_dbus_operation(
                "Suspend inhibitor recreation",
                |pm| {
                    Box::pin(pm.create_suspend_inhibitor(
                        "MQTT daemon running - preventing unexpected suspension",
                    ))
                },
                max_retries,
            )
            .await
        {
            warn!("Failed to recreate suspend inhibitor: {}", e);
        }
    }
}
