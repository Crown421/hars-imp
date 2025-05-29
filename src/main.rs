use rumqttc::{Event, Packet};
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, trace, warn};

pub mod components;
pub mod dbus;
pub mod ha_mqtt;
pub mod shutdown;
pub mod utils;

use dbus::{handle_power_events, setup_power_monitoring};
use ha_mqtt::initialize_mqtt_connection;
use shutdown::{perform_graceful_shutdown, ShutdownHandler};
use utils::{init_tracing, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load_from_file("config.toml")?;

    // Initialize tracing with the configured log level
    init_tracing(&config.log_level)?;

    info!("Starting MQTT daemon for hostname: {}", config.hostname);
    info!(
        "Connecting to MQTT broker: {}:{}",
        config.mqtt_url, config.mqtt_port
    );
    debug!("Log level set to: {}", config.log_level);

    // Set up power monitoring
    let (mut power_manager, _power_monitor_handle) = setup_power_monitoring().await;

    // Initialize MQTT connection
    let (
        mut client,
        mut eventloop,
        mut topic_handlers,
        mut status_manager,
        mut system_monitor_handle,
    ) = initialize_mqtt_connection(&config).await?;

    // Setup shutdown signal handlers
    let mut shutdown_handler = ShutdownHandler::new()?;

    // Main event loop
    info!("Starting main event loop");
    loop {
        tokio::select! {
            res = eventloop.poll() => {
                match res {
                    Ok(notification) => {
                        match notification {
                            Event::Incoming(Packet::Publish(publish)) => {
                                let topic = &publish.topic;
                                let payload = String::from_utf8_lossy(&publish.payload);
                                trace!("Received message on topic '{}': {}", topic, payload);

                                // Check if this message should be handled by our topic handlers
                                match topic_handlers.handle_message(topic, &payload, &client).await {
                                    Ok(true) => {
                                        // Message was handled by a topic handler
                                    }
                                    Ok(false) => {
                                        // Message not handled, treat as regular message
                                        info!("Message on topic '{}': {}", topic, payload);
                                    }
                                    Err(e) => {
                                        error!("Error handling message on topic '{}': {}", topic, e);
                                    }
                                }
                            }
                            event => {
                                // Other events (connections, pings, etc.)
                                debug!("MQTT event: {:?}", event);
                            }
                        }
                    }
                    Err(e) => {
                        error!("MQTT error: {}", e);
                        warn!("Waiting {}ms before retrying", config.update_interval_ms);
                        // Wait a bit before retrying
                        time::sleep(Duration::from_millis(config.update_interval_ms)).await;
                    }
                }
            }
            power_event = handle_power_events(&mut power_manager) => {
                if let Some(event) = power_event {
                    let mut handler = dbus::PowerEventHandler::new(
                        &mut power_manager,
                        &mut client,
                        &mut eventloop,
                        &mut topic_handlers,
                        &mut status_manager,
                        &mut system_monitor_handle,
                        &config,
                    );
                    handler.handle_event(event).await;
                } else {
                    // Power event channel closed, power monitoring stopped
                    debug!("Power monitoring stopped");
                }
            }
            signal = shutdown_handler.wait_for_shutdown_signal() => {
                info!("{}", signal.description());
                perform_graceful_shutdown(&mut status_manager, &mut client, &mut eventloop, Some(&mut power_manager)).await?;
                break;
            }
        }
    }

    info!("MQTT daemon shut down.");
    Ok(())
}
