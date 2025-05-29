use crate::discovery::HomeAssistantComponent;
use crate::utils::Config;
use rumqttc::{AsyncClient, QoS};
use tracing::{debug, error, info};

pub async fn execute_switch_command(
    command: &str,
    state: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    debug!("Executing switch command: {} {}", command, state);
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&format!("{} {}", command, state))
        .output()
        .await?;

    if output.status.success() {
        let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
        debug!("Switch command output: {}", result);
        Ok(result)
    } else {
        let error_msg = format!(
            "Switch command failed with exit code: {:?}",
            output.status.code()
        );
        debug!(
            "Switch command stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Err(error_msg.into())
    }
}

pub async fn handle_switch_command(
    topic: &str,
    payload: &str,
    switch_topics: &[(String, String, String)], // (command_topic, state_topic, exec_command)
    client: &AsyncClient,
) -> bool {
    for (command_topic, state_topic, exec_command) in switch_topics {
        if topic == command_topic {
            let payload = payload.trim();
            if payload == "ON" || payload == "OFF" {
                info!(
                    "Switch command received on topic '{}': {}, executing: {} {}",
                    topic,
                    payload,
                    exec_command,
                    payload.to_lowercase()
                );

                match execute_switch_command(exec_command, &payload.to_lowercase()).await {
                    Ok(_output) => {
                        info!("Switch command executed successfully");
                        // Publish the new state to the state topic
                        if let Err(e) = client
                            .publish(state_topic, QoS::AtLeastOnce, true, payload)
                            .await
                        {
                            error!("Failed to publish switch state: {}", e);
                        } else {
                            debug!(
                                "Published switch state '{}' to topic '{}'",
                                payload, state_topic
                            );
                        }
                    }
                    Err(e) => {
                        error!("Failed to execute switch command '{}': {}", exec_command, e);
                        // Publish empty payload to indicate command failure
                        if let Err(e) = client
                            .publish(state_topic, QoS::AtLeastOnce, true, "")
                            .await
                        {
                            error!("Failed to publish switch failure state: {}", e);
                        } else {
                            debug!(
                                "Published empty state to topic '{}' due to command failure",
                                state_topic
                            );
                        }
                    }
                }
                return true;
            } else {
                debug!(
                    "Ignoring invalid switch payload '{}' on topic '{}'",
                    payload, topic
                );
            }
        }
    }
    false
}

/// Creates switch components and returns switch topics for subscription
pub async fn create_switch_components_and_setup(
    client: &AsyncClient,
    config: &Config,
) -> Result<(Vec<(String, HomeAssistantComponent)>, Vec<(String, String, String)>), Box<dyn std::error::Error>> {
    let mut switch_components = Vec::new();
    let mut switch_topics = Vec::new();

    if let Some(switches) = &config.switch {
        debug!("Setting up {} switch(es)", switches.len());
        for switch in switches {
            let switch_id = format!(
                "{}_{}",
                config.hostname,
                switch.name.replace(" ", "_").to_lowercase()
            );

            let command_topic = format!("homeassistant/switch/{}/set", switch_id);
            let state_topic = format!("homeassistant/switch/{}/state", switch_id);

            // Create component
            let component = HomeAssistantComponent::switch(
                switch.name.clone(),
                switch_id.clone(),
                command_topic.clone(),
                state_topic.clone(),
            );
            
            switch_components.push((switch_id, component));

            // Subscribe to switch command topic
            debug!("Subscribing to switch command topic: {}", command_topic);
            client.subscribe(&command_topic, QoS::AtMostOnce).await?;

            switch_topics.push((command_topic, state_topic, switch.exec.clone()));
        }
    }

    Ok((switch_components, switch_topics))
}
