// Power management module - handles power events and system state management

use rumqttc::AsyncClient;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::status::StatusManager;
use crate::Config;
use super::inhibitor::PowerManager;

/// Power event types that can be received from the system
#[derive(Debug, Clone)]
pub enum PowerEvent {
    Suspending,
    Resuming,
}

/// Setup function to initialize power monitoring
/// Returns a PowerManager instance and starts the monitoring task
pub async fn setup_power_monitoring() -> (PowerManager, tokio::task::JoinHandle<()>) {
    let power_manager = PowerManager::new();
    
    // Create a separate instance for the background monitoring task
    let mut monitor_instance = PowerManager::new_with_sender(power_manager.clone_sender());
    
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
    match power_manager.get_receiver().recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            debug!("Power event channel closed");
            None
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!("Power event receiver lagged, skipped {} events", skipped);
            // Try to receive the next event
            match power_manager.get_receiver().recv().await {
                Ok(event) => Some(event),
                Err(_) => None,
            }
        }
    }
}

/// Handler for power events that encapsulates all power management actions
pub struct PowerEventHandler<'a> {
    power_manager: &'a mut PowerManager,
    client: &'a mut AsyncClient,
    eventloop: &'a mut rumqttc::EventLoop,
    button_topics: &'a mut Vec<(String, String)>,
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
        button_topics: &'a mut Vec<(String, String)>,
        status_manager: &'a mut StatusManager,
        system_monitor_handle: &'a mut tokio::task::JoinHandle<()>,
        config: &'a Config,
    ) -> Self {
        Self {
            power_manager,
            client,
            eventloop,
            button_topics,
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
        
        // Perform critical shutdown actions
        if let Err(e) = self.status_manager.publish_suspended().await {
            error!("Failed to publish suspend status: {}", e);
        } else {
            debug!("Successfully published 'Suspended' status before suspend");
        }
        
        // Stop system monitoring
        self.system_monitor_handle.abort();
        debug!("Stopped system monitoring");
        
        // Gracefully disconnect MQTT client
        info!("Disconnecting MQTT client before suspend");
        self.client.disconnect().await.unwrap_or_else(|e| {
            warn!("Error during MQTT disconnect: {}", e);
        });
        debug!("MQTT client disconnected");
        
        // Release the inhibitor to allow the system to suspend
        self.power_manager.release_inhibitor();
        debug!("Pre-suspend actions completed, released inhibitor to allow system suspend");
    }

    /// Handle system resume by re-establishing connections and services
    async fn handle_resume(&mut self) {
        info!("System resumed from suspend, re-establishing connections...");
        
        // Recreate the suspend inhibitor for future suspension events
        if let Err(e) = self.power_manager.create_inhibitor("MQTT daemon running - preventing unexpected suspension").await {
            warn!("Failed to recreate suspend inhibitor after resume: {}", e);
        } else {
            debug!("Recreated suspend inhibitor after resume");
        }
        
        // Re-initialize MQTT connection
        info!("Re-initializing MQTT connection after resume");
        match crate::initialize_mqtt_connection(self.config).await {
            Ok((new_client, new_eventloop, new_button_topics, new_status_manager, new_monitoring_handle)) => {
                *self.client = new_client;
                *self.eventloop = new_eventloop;
                *self.button_topics = new_button_topics;
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
    }
}
