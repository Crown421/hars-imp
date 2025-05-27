use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use std::time::Duration;
use tokio::time;
use tokio::signal;
use tracing::{info, warn, error, debug, trace};

pub mod config;
pub mod commands;
pub mod discovery;
pub mod logging;
pub mod status;
pub mod system_monitor;
pub mod version;
pub mod dbus;

use config::Config;
use commands::handle_button_press;
use discovery::{setup_button_discovery, setup_sensor_discovery, setup_status_discovery};
use logging::init_tracing;
use status::StatusManager;
use system_monitor::SystemMonitor;
use dbus::{PowerMonitor, PowerEvent}; 

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
    let mut system_monitor = SystemMonitor::new(config.hostname.clone(), client.clone());
    
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

    // Set up power monitor
    let power_monitor = PowerMonitor::new();
    let mut power_event_rx = power_monitor.subscribe();
    
    // Clone power monitor for the background task
    let mut power_monitor_bg = PowerMonitor::new();
    
    // Start power monitor in background
    let _power_monitor_handle = tokio::spawn(async move {
        if let Err(e) = power_monitor_bg.run().await {
            warn!("Power monitor encountered an error: {}", e);
            warn!("Power monitoring functionality will be unavailable.");
        }
    });
    
    // Initialize MQTT connection
    let (mut client, mut eventloop, mut button_topics, mut status_manager, mut system_monitor_handle) = 
        initialize_mqtt_connection(&config).await?;
    
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
            result = power_event_rx.recv() => {
                match result {
                    Ok(PowerEvent::Suspending) => {
                        info!("System is about to suspend, performing shutdown actions...");
                        
                        // Create a suspend inhibitor to delay suspension
                        let _inhibitor = match power_monitor.create_inhibitor("Saving MQTT state before suspend").await {
                            Ok(inhibitor) => {
                                debug!("Created suspend inhibitor, delaying system suspend");
                                Some(inhibitor)
                            }
                            Err(e) => {
                                warn!("Failed to create suspend inhibitor: {}", e);
                                None
                            }
                        };
                        
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
                        
                        // The inhibitor will be automatically released when it goes out of scope
                        debug!("Pre-suspend actions completed, allowing system to suspend");
                    }
                    Ok(PowerEvent::Resuming) => {
                        info!("System resumed from suspend, re-establishing connections...");
                        
                        // Re-initialize MQTT connection
                        info!("Re-initializing MQTT connection after resume");
                        match initialize_mqtt_connection(&config).await {
                            Ok((new_client, new_eventloop, new_button_topics, new_status_manager, new_monitoring_handle)) => {
                                client = new_client;
                                eventloop = new_eventloop;
                                button_topics = new_button_topics;
                                status_manager = new_status_manager;
                                system_monitor_handle = new_monitoring_handle;
                                
                                info!("MQTT connection re-established successfully");
                                debug!("Successfully published 'On' status after resume");
                            }
                            Err(e) => {
                                error!("Failed to re-establish MQTT connection after resume: {}", e);
                                // Continue with the old connection and hope it recovers
                            }
                        }
                        
                        // Add any other post-resume actions here
                    }
                    Err(e) => {
                        error!("Error receiving power event: {}", e);
                    }
                }
            }
            _ = signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down gracefully.");
                // Publish "Off" status before shutting down
                if let Err(e) = status_manager.publish_off().await {
                    error!("Failed to publish off status: {}", e);
                }
                break;
            }
        }
    }

    // Ensure we publish "Off" status before exiting
    if let Err(e) = status_manager.publish_off().await {
        error!("Failed to publish final off status: {}", e);
    }
    
    info!("MQTT daemon shut down.");
    Ok(())
}
