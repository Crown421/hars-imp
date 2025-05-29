use rumqttc::{AsyncClient, Event, MqttOptions, Packet};
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, trace, warn};

pub mod buttons;
pub mod dbus;
pub mod discovery;
pub mod shutdown;
pub mod status;
pub mod switch;
pub mod system_monitor;
pub mod utils;

use buttons::{handle_button_press, setup_button_discovery};
use dbus::{handle_power_events, setup_power_monitoring};
use shutdown::{perform_graceful_shutdown, ShutdownHandler};
use status::{setup_status_discovery, StatusManager};
use switch::{handle_switch_command, setup_switch_discovery};
use system_monitor::{setup_sensor_discovery, SystemMonitor};
use utils::{init_tracing, Config};

async fn initialize_mqtt_connection(
    config: &Config,
) -> Result<
    (
        AsyncClient,
        rumqttc::EventLoop,
        Vec<(String, String)>,
        Vec<(String, String, String)>,
        StatusManager,
        tokio::task::JoinHandle<()>,
    ),
    Box<dyn std::error::Error>,
> {
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    // Create MQTT client
    debug!("Creating MQTT client");
    let (client, eventloop) = AsyncClient::new(mqttoptions, 10);
    debug!("MQTT client created successfully");

    // Handle button discovery and subscription
    let button_topics = setup_button_discovery(&client, &config).await?;

    // Handle switch discovery and subscription
    let switch_topics = setup_switch_discovery(&client, &config).await?;

    // Setup sensor discovery for system monitoring
    setup_sensor_discovery(&client, &config).await?;

    // Setup status sensor discovery
    setup_status_discovery(&client, &config).await?;

    info!("Discovery complete, briefly waiting...");
    time::sleep(Duration::from_millis(500)).await;

    // Create status manager and publish initial status
    debug!("Creating status manager");
    let status_manager = StatusManager::new(config.hostname.clone(), client.clone());
    debug!("Publishing initial 'On' status");
    if let Err(e) = status_manager.publish_on().await {
        warn!("Failed to publish initial status: {}", e);
    } else {
        debug!("Successfully published initial status");
    }

    // Create system monitor
    info!("Starting system monitor");
    let mut system_monitor = SystemMonitor::new(config.sensor_topic_base.clone(), client.clone());

    // Start system monitoring in background
    let monitoring_handle = tokio::spawn(async move {
        system_monitor.run_monitoring_loop().await;
    });

    Ok((
        client,
        eventloop,
        button_topics,
        switch_topics,
        status_manager,
        monitoring_handle,
    ))
}

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
        mut button_topics,
        mut switch_topics,
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

                                // Check if this is a button press
                                let button_handled = handle_button_press(topic, &payload, &button_topics).await;

                                // Check if this is a switch command
                                let switch_handled = if !button_handled {
                                    handle_switch_command(topic, &payload, &switch_topics, &client).await
                                } else {
                                    false
                                };

                                // If not a button press or switch command, treat as regular message
                                if !button_handled && !switch_handled {
                                    info!("Message on topic '{}': {}", topic, payload);
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
                        &mut button_topics,
                        &mut switch_topics,
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
