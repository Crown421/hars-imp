use rumqttc::{AsyncClient, QoS};
use tracing::{debug, error, info};
use crate::discovery::{publish_discovery, create_shared_device, HomeAssistantDiscovery};
use crate::utils::Config;

pub async fn execute_command(command: &str) -> Result<String, Box<dyn std::error::Error>> {
    debug!("Executing command: {}", command);
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await?;
    
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

pub async fn handle_button_press(
    topic: &str,
    payload: &str,
    button_topics: &[(String, String)],
) -> bool {
    for (button_topic, exec_command) in button_topics {
        if topic == button_topic && payload.trim() == "PRESS" {
            info!("Button press detected on topic '{}', executing: {}", topic, exec_command);
            
            match execute_command(exec_command).await {
                Ok(output) => {
                    info!("Command executed successfully: {}", output);
                }
                Err(e) => {
                    error!("Failed to execute command '{}': {}", exec_command, e);
                }
            }
            return true;
        }
    }
    false
}

pub async fn setup_button_discovery(
    client: &AsyncClient,
    config: &Config,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
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
                name: button.name.clone(),
                command_topic: button_topic.clone(),
                unique_id: format!("{}_{}", config.hostname, button.name.replace(" ", "_").to_lowercase()),
                device: create_shared_device(config),
            };
            
            // Publish discovery message
            info!("Publishing discovery for button '{}'", button.name);
            publish_discovery(client, &discovery_topic, &discovery_message, true).await?;
            
            // Subscribe to button command topic
            info!("Subscribing to button topic: {}", button_topic);
            client.subscribe(&button_topic, QoS::AtMostOnce).await?;
            
            button_topics.push((button_topic, button.exec.clone()));
        }
    }
    
    Ok(button_topics)
}
