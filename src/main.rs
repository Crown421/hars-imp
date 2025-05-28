use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use std::time::Duration;
use tokio::time;
use tracing::{info, warn, error, debug, trace};

pub mod utils;
pub mod commands;
pub mod discovery;
pub mod status;
pub mod system_monitor;
pub mod dbus;
pub mod shutdown;

use utils::{Config, init_tracing};
use commands::{handle_button_press, setup_button_discovery};
use status::{StatusManager, setup_status_discovery};
use system_monitor::{SystemMonitor, setup_sensor_discovery};
use dbus::{setup_power_monitoring, handle_power_events};
use shutdown::{ShutdownHandler, perform_graceful_shutdown}; 

async fn initialize_mqtt_connection(config: &Config) -> Result<(AsyncClient, rumqttc::EventLoop, Vec<(String, String)>, StatusManager, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    
    // Create MQTT client
    debug!("Creating MQTT client");
    let (client, eventloop) = AsyncClient::new(mqttoptions, 10);
    debug!("MQTT client created successfully");
    
    // Subscribe to regular topics
    for topic in &config.topics {
        info!("Subscribing to topic: {}", topic);
        client.subscribe(topic, QoS::AtMostOnce).await?;
    }
    debug!("Subscribed to all regular topics");
    
    // Handle button discovery and subscription
    debug!("Setting up button discovery");
    let button_topics = setup_button_discovery(&client, &config).await?;
    debug!("Button discovery completed");
    
    // Setup sensor discovery for system monitoring
    debug!("Setting up sensor discovery");
    setup_sensor_discovery(&client, &config).await?;
    debug!("Sensor discovery completed");
    
    // Setup status sensor discovery
    debug!("Setting up status discovery");
    setup_status_discovery(&client, &config).await?;
    debug!("Status discovery completed");

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
    
    Ok((client, eventloop, button_topics, status_manager, monitoring_handle))
} 

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load_from_file("config.toml")?;
    
    // Initialize tracing with the configured log level
    init_tracing(&config.log_level)?;
    
    info!("Starting MQTT daemon for hostname: {}", config.hostname);
    info!("Connecting to MQTT broker: {}:{}", config.mqtt_url, config.mqtt_port);
    debug!("Log level set to: {}", config.log_level);

    // Set up power monitoring
    let (mut power_manager, _power_monitor_handle) = setup_power_monitoring().await;
    
    // Initialize MQTT connection
    let (mut client, mut eventloop, mut button_topics, mut status_manager, mut system_monitor_handle) = 
        initialize_mqtt_connection(&config).await?;
    
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
                                
                                // If not a button press, treat as regular message
                                if !button_handled {
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
