use rumqttc::{AsyncClient, QoS};

/// Unified topic management for all component types
#[derive(Debug, Clone)]
pub enum TopicHandler {
    Button {
        topic: String,
        exec_command: String,
    },
    Switch {
        command_topic: String,
        state_topic: String,
        exec_command: String,
    },
}

/// Container for all topics that need to be handled
#[derive(Debug, Default)]
pub struct TopicHandlers {
    pub handlers: Vec<TopicHandler>,
}

impl TopicHandlers {
    /// Creates a new empty TopicHandlers instance.
    ///
    /// # Returns
    /// * `Self` - A new TopicHandlers instance with no registered handlers
    ///
    /// # Examples
    /// ```
    /// let handlers = TopicHandlers::new();
    /// ```
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn add_button(&mut self, topic: String, exec_command: String) {
        self.handlers.push(TopicHandler::Button {
            topic,
            exec_command,
        });
    }

    pub fn add_switch(&mut self, command_topic: String, state_topic: String, exec_command: String) {
        self.handlers.push(TopicHandler::Switch {
            command_topic,
            state_topic,
            exec_command,
        });
    }

    /// Handle an incoming MQTT message and return true if handled
    pub async fn handle_message(
        &self,
        topic: &str,
        payload: &str,
        client: &AsyncClient,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        use crate::components::buttons::execute_command;
        use crate::components::switch::execute_switch_command;
        use tracing::{debug, error, info};

        for handler in &self.handlers {
            match handler {
                TopicHandler::Button {
                    topic: button_topic,
                    exec_command,
                } => {
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
                        return Ok(true);
                    }
                }
                TopicHandler::Switch {
                    command_topic,
                    state_topic,
                    exec_command,
                } => {
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

                            match execute_switch_command(exec_command, &payload.to_lowercase())
                                .await
                            {
                                Ok(_output) => {
                                    info!("Switch command executed successfully");
                                    // Publish the new state to the state topic
                                    client
                                        .publish(state_topic, QoS::AtLeastOnce, true, payload)
                                        .await?;
                                    debug!(
                                        "Published switch state '{}' to topic '{}'",
                                        payload, state_topic
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to execute switch command '{}': {}",
                                        exec_command, e
                                    );
                                    // Publish empty payload to indicate command failure
                                    client
                                        .publish(state_topic, QoS::AtLeastOnce, true, "")
                                        .await?;
                                    debug!("Published empty state to topic '{}' due to command failure", state_topic);
                                }
                            }
                            return Ok(true);
                        } else {
                            debug!(
                                "Ignoring invalid switch payload '{}' on topic '{}'",
                                payload, topic
                            );
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// Get all topics that need to be subscribed to
    pub fn get_subscription_topics(&self) -> Vec<String> {
        let mut topics = Vec::new();
        for handler in &self.handlers {
            match handler {
                TopicHandler::Button { topic, .. } => {
                    topics.push(topic.clone());
                }
                TopicHandler::Switch { command_topic, .. } => {
                    topics.push(command_topic.clone());
                }
            }
        }
        topics
    }
}
