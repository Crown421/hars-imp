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
