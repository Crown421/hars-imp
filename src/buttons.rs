use crate::ha_mqtt::HomeAssistantComponent;
use crate::utils::Config;
use rumqttc::{AsyncClient, QoS};
use tracing::{debug, error, info};

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
        debug!(
            "Command stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
            info!(
                "Button press detected on topic '{}', executing: {}",
                topic, exec_command
            );

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

/// Creates button components and returns button topics for subscription
pub async fn create_button_components_and_setup(
    client: &AsyncClient,
    config: &Config,
) -> Result<
    (Vec<(String, HomeAssistantComponent)>, Vec<(String, String)>),
    Box<dyn std::error::Error>,
> {
    let mut button_components = Vec::new();
    let mut button_topics = Vec::new();

    if let Some(buttons) = &config.button {
        debug!("Setting up {} button(s)", buttons.len());
        for button in buttons {
            let button_id = format!(
                "{}_{}",
                config.hostname,
                button.name.replace(" ", "_").to_lowercase()
            );
            let button_topic = format!("homeassistant/button/{}/set", button_id);

            // Create component
            let component = HomeAssistantComponent::button(
                button.name.clone(),
                button_id.clone(),
                button_topic.clone(),
            );

            button_components.push((button_id, component));

            // Subscribe to button command topic
            debug!("Subscribing to button topic: {}", button_topic);
            client.subscribe(&button_topic, QoS::AtMostOnce).await?;

            button_topics.push((button_topic, button.exec.clone()));
        }
    }

    Ok((button_components, button_topics))
}
