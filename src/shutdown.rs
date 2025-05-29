use crate::dbus::{PowerManager, StatusManager};
use rumqttc::{AsyncClient, EventLoop};
use std::time::Duration;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::time;
use tracing::{debug, error, info};

pub struct ShutdownHandler {
    sigterm: Signal,
    sigint: Signal,
}

impl ShutdownHandler {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let sigterm = signal(SignalKind::terminate())?;
        let sigint = signal(SignalKind::interrupt())?;

        Ok(ShutdownHandler { sigterm, sigint })
    }

    pub async fn wait_for_shutdown_signal(&mut self) -> ShutdownSignal {
        tokio::select! {
            _ = self.sigint.recv() => ShutdownSignal::Interrupt,
            _ = self.sigterm.recv() => ShutdownSignal::Terminate,
        }
    }
}

#[derive(Debug)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl ShutdownSignal {
    pub fn description(&self) -> &'static str {
        match self {
            ShutdownSignal::Interrupt => "SIGINT (Ctrl+C) received",
            ShutdownSignal::Terminate => "SIGTERM received (likely from systemctl)",
        }
    }
}

/// Shutdown scenario types for different contexts
#[derive(Debug, Clone)]
pub enum ShutdownScenario {
    /// Full application shutdown (e.g., SIGTERM, SIGINT)
    FullShutdown,
    /// System suspend - application will resume later
    Suspend,
}

impl ShutdownScenario {
    pub fn description(&self) -> &'static str {
        match self {
            ShutdownScenario::FullShutdown => "full shutdown",
            ShutdownScenario::Suspend => "suspend",
        }
    }
}

/// Gracefully shut down MQTT connection with proper event queue draining
/// This function can be used for both full shutdown and suspend scenarios
pub async fn perform_graceful_mqtt_shutdown(
    status_manager: &mut StatusManager,
    client: &mut AsyncClient,
    eventloop: &mut EventLoop,
    scenario: ShutdownScenario,
) -> Result<(), Box<dyn std::error::Error>> {
    info!(
        "Performing graceful MQTT shutdown for {}...",
        scenario.description()
    );

    // Publish appropriate status message based on scenario
    let status_result = match scenario {
        ShutdownScenario::FullShutdown => status_manager.publish_off().await,
        ShutdownScenario::Suspend => status_manager.publish_suspended().await,
    };

    if let Err(e) = status_result {
        error!("Failed to publish {} status: {}", scenario.description(), e);
    } else {
        info!(
            "{} status message queued successfully",
            scenario.description()
        );
    }

    // Process any pending events to ensure message is sent
    info!("Processing final MQTT events to drain queue...");
    let max_attempts = 2;
    for i in 0..max_attempts {
        debug!(
            "Processing {} events (attempt {}/{})",
            scenario.description(),
            i + 1,
            max_attempts
        );
        match eventloop.poll().await {
            Ok(event) => {
                debug!("Processing {} event: {:?}", scenario.description(), event);
            }
            Err(e) => {
                debug!(
                    "Event processing error during {}: {}",
                    scenario.description(),
                    e
                );
                break;
            }
        }
        time::sleep(Duration::from_millis(5)).await;
    }

    // Explicitly disconnect the MQTT client
    info!("Disconnecting from MQTT broker...");
    match client.disconnect().await {
        Ok(_) => debug!("MQTT client disconnected cleanly"),
        Err(e) => {
            // Handle expected disconnect errors more gracefully
            if e.to_string().contains("connection closed by peer")
                || e.to_string().contains("ConnectionAborted")
            {
                debug!(
                    "Expected disconnect behavior during {}: {}",
                    scenario.description(),
                    e
                );
            } else {
                error!("Error disconnecting from MQTT broker: {}", e);
            }
        }
    }

    info!(
        "Graceful MQTT shutdown for {} completed",
        scenario.description()
    );
    Ok(())
}

/// Perform complete graceful shutdown for full application termination
pub async fn perform_graceful_shutdown(
    status_manager: &mut StatusManager,
    client: &mut AsyncClient,
    eventloop: &mut EventLoop,
    power_manager: Option<&mut PowerManager>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Performing graceful shutdown...");

    // Release shutdown inhibitor first to signal we're handling the shutdown
    if let Some(pm) = power_manager {
        pm.release_shutdown_inhibitor();
        debug!("Released shutdown inhibitor to acknowledge shutdown signal");
    }

    // Use the general MQTT shutdown function
    perform_graceful_mqtt_shutdown(
        status_manager,
        client,
        eventloop,
        ShutdownScenario::FullShutdown,
    )
    .await?;

    info!("Graceful shutdown completed");
    Ok(())
}
