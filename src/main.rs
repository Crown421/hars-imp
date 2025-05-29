use rumqttc::{AsyncClient, Event, MqttOptions, Packet};
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info, trace, warn};

pub mod components;
pub mod dbus;
pub mod ha_mqtt;
pub mod shutdown;
pub mod utils;

use components::{
    create_button_components_and_setup, create_switch_components_and_setup,
    create_system_sensor_components, SystemMonitor,
};
use dbus::{create_status_component, handle_power_events, setup_power_monitoring, StatusManager};
use ha_mqtt::{publish_unified_discovery, TopicHandlers};
use shutdown::{perform_graceful_shutdown, ShutdownHandler};
use utils::{init_tracing, Config};

async fn initialize_mqtt_connection(
    config: &Config,
) -> Result<
    (
        AsyncClient,
        rumqttc::EventLoop,
        TopicHandlers,
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

    // Collect all components for unified discovery
    let mut all_components = Vec::new();
    let mut topic_handlers = TopicHandlers::new();

    // Handle button components and subscriptions
    let (button_components, button_topics) =
        create_button_components_and_setup(&client, config).await?;
    all_components.extend(button_components);

    // Add button topics to unified handlers
    for (topic, exec_command) in button_topics {
        topic_handlers.add_button(topic, exec_command);
    }

    // Handle switch components and subscriptions
    let (switch_components, switch_topics) =
        create_switch_components_and_setup(&client, config).await?;
    all_components.extend(switch_components);

    // Add switch topics to unified handlers
    for (command_topic, state_topic, exec_command) in switch_topics {
        topic_handlers.add_switch(command_topic, state_topic, exec_command);
    }

    // Create system monitoring sensor components
    let system_components = create_system_sensor_components(config);
    all_components.extend(system_components);

    // Create status sensor component
    let (status_id, status_component) = create_status_component(config);
    all_components.push((status_id, status_component));

    // Publish unified device discovery with all components
    info!(
        "Publishing unified device discovery with {} components",
        all_components.len()
    );
    publish_unified_discovery(&client, config, all_components).await?;

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
        topic_handlers,
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
