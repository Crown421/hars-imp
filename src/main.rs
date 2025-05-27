use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Duration;
use std::process::Command;
use tokio::time;
use tracing::{info, warn, error, debug, trace};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Deserialize, Debug)]
struct Button {
    name: String,
    exec: String,
}

#[derive(Deserialize, Debug)]
struct Config {
    hostname: String,
    mqtt_url: String,
    mqtt_port: u16,
    username: String,
    password: String,
    log_level: String,
    topics: Vec<String>,
    update_interval_ms: u64,
    button: Option<Vec<Button>>,
}

#[derive(Serialize)]
struct HomeAssistantDiscovery {
    name: String,
    command_topic: String,
    unique_id: String,
    device: HomeAssistantDevice,
}

#[derive(Serialize)]
struct HomeAssistantDevice {
    identifiers: Vec<String>,
    name: String,
    model: String,
    manufacturer: String,
}

impl Config {
    fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }
}

fn init_tracing(log_level: &str) -> Result<(), Box<dyn std::error::Error>> {
    let filter = EnvFilter::try_new(log_level)
        .or_else(|_| EnvFilter::try_new("info"))?;
    
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
    
    Ok(())
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
    
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    
    // Create MQTT client
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    
    // Subscribe to regular topics
    for topic in &config.topics {
        info!("Subscribing to topic: {}", topic);
        client.subscribe(topic, QoS::AtMostOnce).await?;
    }
    
    // Handle button discovery and subscription
    let mut button_topics = Vec::new();
    if let Some(buttons) = &config.button {
        debug!("Setting up {} button(s)", buttons.len());
        for button in buttons {
            let button_topic = format!("homeassistant/button/{}/set", 
                                     format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()));
            let discovery_topic = format!("homeassistant/button/{}/config", 
                                        format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()));
            
            // Create discovery message
            let discovery_message = HomeAssistantDiscovery {
                name: button.name.clone(), // Just use the button name, not hostname + button name
                command_topic: button_topic.clone(),
                unique_id: format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()),
                device: HomeAssistantDevice {
                    identifiers: vec![config.hostname.clone()],
                    name: config.hostname.clone(),
                    model: "MQTT Daemon".to_string(),
                    manufacturer: "Custom".to_string(),
                },
            };
            
            let discovery_json = serde_json::to_string(&discovery_message)?;
            
            // Publish discovery message
            info!("Publishing discovery for button '{}' to: {}", button.name, discovery_topic);
            debug!("Discovery payload: {}", discovery_json);
            client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;
            
            // Subscribe to button command topic
            info!("Subscribing to button topic: {}", button_topic);
            client.subscribe(&button_topic, QoS::AtMostOnce).await?;
            
            button_topics.push((button_topic, button.exec.clone()));
        }
    }
    
    // Main event loop
    info!("Starting main event loop");
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let topic = &publish.topic;
                let payload = String::from_utf8_lossy(&publish.payload);
                trace!("Received message on topic '{}': {}", topic, payload);
                
                // Check if this is a button press
                let mut button_handled = false;
                for (button_topic, exec_command) in &button_topics {
                    if topic == button_topic && payload.trim() == "PRESS" {
                        info!("Button press detected on topic '{}', executing: {}", topic, exec_command);
                        
                        // Execute the command
                        match execute_command(exec_command) {
                            Ok(output) => {
                                info!("Command executed successfully: {}", output);
                            }
                            Err(e) => {
                                error!("Failed to execute command '{}': {}", exec_command, e);
                            }
                        }
                        button_handled = true;
                        break;
                    }
                }
                
                // If not a button press, treat as regular message
                if !button_handled {
                    info!("Message on topic '{}': {}", topic, payload);
                }
            }
            Ok(event) => {
                // Other events (connections, pings, etc.)
                debug!("MQTT event: {:?}", event);
            }
            Err(e) => {
                error!("MQTT error: {}", e);
                warn!("Waiting {}ms before retrying", config.update_interval_ms);
                // Wait a bit before retrying
                time::sleep(Duration::from_millis(config.update_interval_ms)).await;
            }
        }
    }
}

fn execute_command(command: &str) -> Result<String, Box<dyn std::error::Error>> {
    debug!("Executing command: {}", command);
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()?;
    
    if output.status.success() {
        let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
        debug!("Command output: {}", result);
        Ok(result)
    } else {
        let error_msg = format!("Command failed with exit code: {:?}", output.status.code());
        debug!("Command stderr: {}", String::from_utf8_lossy(&output.stderr));
        Err(error_msg.into())
    }
}
