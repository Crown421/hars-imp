use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use std::time::Duration;
use tokio::time;
use tokio::signal; // Add this line
use tracing::{info, warn, error, debug, trace};

pub mod config;
pub mod commands;
pub mod discovery;
pub mod logging;
pub mod system_monitor;

use config::Config;
use commands::handle_button_press;
use discovery::{setup_button_discovery, setup_sensor_discovery, StateManager};
use logging::init_tracing;
use system_monitor::SystemMonitor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load_from_file("config.toml")?;
    
    // Initialize tracing with the configured log level
    init_tracing(&config.log_level)?;
    
    info!("Starting MQTT daemon for hostname: {}", config.hostname);
    info!("Connecting to MQTT broker: {}:{}", config.mqtt_url, config.mqtt_port);
    debug!("Log level set to: {}", config.log_level);
    
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    
    // Create MQTT client
    debug!("Creating MQTT client");
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
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
    
    // Create state manager and publish initial state
    // debug!("Creating state manager");
    // let state_manager = StateManager::new(config.hostname.clone(), client.clone());
    // debug!("Publishing initial 'On' state");
    // if let Err(e) = state_manager.publish_on().await {
    //     warn!("Failed to publish initial state: {}", e);
    // } else {
    //     debug!("Successfully published initial state");
    // }
    
    // Create system monitor
    info!("Starting system monitor");
    let mut system_monitor = SystemMonitor::new(config.hostname.clone(), client.clone());
    
    // Start system monitoring in background
    let _monitoring_handle = tokio::spawn(async move {
        system_monitor.run_monitoring_loop().await;
    });
    
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
            _ = signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down gracefully.");
                // Publish "Off" state before shutting down
                // if let Err(e) = state_manager.publish_off().await {
                //     error!("Failed to publish off state: {}", e);
                // }
                break;
            }
        }
    }

    // // Ensure we publish "Off" state before exiting
    // if let Err(e) = state_manager.publish_off().await {
    //     error!("Failed to publish final off state: {}", e);
    // }
    
    info!("MQTT daemon shut down.");
    Ok(())
}
