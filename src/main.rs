use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use serde::{Deserialize, Serialize};
use std::fs;
use std::time::Duration;
use std::process::Command;
use tokio::time;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration
    let config = Config::load_from_file("config.toml")?;
    
    println!("Starting MQTT daemon for hostname: {}", config.hostname);
    println!("Connecting to MQTT broker: {}:{}", config.mqtt_url, config.mqtt_port);
    
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    
    // Create MQTT client
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    
    // Subscribe to regular topics
    for topic in &config.topics {
        println!("Subscribing to topic: {}", topic);
        client.subscribe(topic, QoS::AtMostOnce).await?;
    }
    
    // Handle button discovery and subscription
    let mut button_topics = Vec::new();
    if let Some(buttons) = &config.button {
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
            println!("Publishing discovery for button '{}' to: {}", button.name, discovery_topic);
            client.publish(&discovery_topic, QoS::AtLeastOnce, true, discovery_json).await?;
            
            // Subscribe to button command topic
            println!("Subscribing to button topic: {}", button_topic);
            client.subscribe(&button_topic, QoS::AtMostOnce).await?;
            
            button_topics.push((button_topic, button.exec.clone()));
        }
    }
    
    // Main event loop
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let topic = &publish.topic;
                let payload = String::from_utf8_lossy(&publish.payload);
                
                // Check if this is a button press
                let mut button_handled = false;
                for (button_topic, exec_command) in &button_topics {
                    if topic == button_topic && payload.trim() == "PRESS" {
                        println!("[{}] Button press detected on topic '{}', executing: {}", 
                            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                            topic, 
                            exec_command
                        );
                        
                        // Execute the command
                        match execute_command(exec_command) {
                            Ok(output) => {
                                println!("Command executed successfully: {}", output);
                            }
                            Err(e) => {
                                eprintln!("Failed to execute command '{}': {}", exec_command, e);
                            }
                        }
                        button_handled = true;
                        break;
                    }
                }
                
                // If not a button press, treat as regular message
                if !button_handled {
                    println!("[{}] Message on topic '{}': {}", 
                        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                        topic, 
                        payload
                    );
                }
            }
            Ok(_) => {
                // Other events (connections, pings, etc.) - we can ignore for now
            }
            Err(e) => {
                eprintln!("MQTT error: {}", e);
                // Wait a bit before retrying
                time::sleep(Duration::from_millis(config.update_interval_ms)).await;
            }
        }
    }
}

fn execute_command(command: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()?;
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!("Command failed with exit code: {:?}", output.status.code()).into())
    }
}
