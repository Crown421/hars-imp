use rumqttc::{MqttOptions, AsyncClient, QoS, Event, Packet};
use serde::Deserialize;
use std::fs;
use std::time::Duration;
use tokio::time;

#[derive(Deserialize, Debug)]
struct Config {
    hostname: String,
    mqtt_url: String,
    mqtt_port: u16,
    username: String,
    password: String,
    topics: Vec<String>,
    update_interval_ms: u64,
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
    
    // Subscribe to topics
    for topic in &config.topics {
        println!("Subscribing to topic: {}", topic);
        client.subscribe(topic, QoS::AtMostOnce).await?;
    }
    
    // Main event loop
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let topic = &publish.topic;
                let payload = String::from_utf8_lossy(&publish.payload);
                println!("[{}] Message on topic '{}': {}", 
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                    topic, 
                    payload
                );
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
