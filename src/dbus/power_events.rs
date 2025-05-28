// Power event handling - public API for power management

use tokio::sync::broadcast;
use tracing::{error, info, warn, debug};
use rumqttc::AsyncClient;

use super::inhibitor::PowerManager;

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
