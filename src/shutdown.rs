use rumqttc::{AsyncClient, EventLoop};
use std::time::Duration;
use tokio::time;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tracing::{info, error, debug};
use crate::status::StatusManager;

pub struct ShutdownHandler {
    sigterm: Signal,
    sigint: Signal,
}

impl ShutdownHandler {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let sigterm = signal(SignalKind::terminate())?;
        let sigint = signal(SignalKind::interrupt())?;
        
        Ok(ShutdownHandler {
            sigterm,
            sigint,
        })
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

pub async fn perform_graceful_shutdown(
    status_manager: &mut StatusManager,
    client: &mut AsyncClient,
    eventloop: &mut EventLoop,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Performing graceful shutdown...");
    
    // Publish "Off" status
    if let Err(e) = status_manager.publish_off().await {
        error!("Failed to publish off status: {}", e);
    } else {
        info!("Off status message queued successfully");
    }
    
    // Process any pending events to ensure message is sent
    info!("Processing final MQTT events...");
    for i in 0..5 {
        debug!("Processing shutdown events (attempt {}/5)", i + 1);
        match eventloop.poll().await {
            Ok(event) => {
                debug!("Processing shutdown event: {:?}", event);
            }
            Err(e) => {
                debug!("Event processing error during shutdown: {}", e);
                break;
            }
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    
    // Explicitly disconnect the MQTT client
    info!("Disconnecting from MQTT broker...");
    if let Err(e) = client.disconnect().await {
        error!("Error disconnecting from MQTT broker: {}", e);
    } else {
        debug!("Successfully disconnected from MQTT broker");
    }
    
    info!("Graceful shutdown completed");
    Ok(())
}
