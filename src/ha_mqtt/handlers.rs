use crate::utils::config::DBusAction;
use rumqttc::{AsyncClient, QoS};

#[derive(Debug, Clone)]
pub enum SwitchAction {
    Exec(String),
    DBus(DBusAction),
}

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
        action: SwitchAction,
    },
    Notification {
        topic: String,
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

    pub fn add_switch(&mut self, command_topic: String, state_topic: String, action: SwitchAction) {
        self.handlers.push(TopicHandler::Switch {
            command_topic,
            state_topic,
            action,
        });
    }

    pub fn add_notification(&mut self, topic: String) {
        self.handlers.push(TopicHandler::Notification { topic });
    }

    /// Handle an incoming MQTT message and return true if handled
    pub async fn handle_message(
        &self,
        topic: &str,
        payload: &str,
        client: &AsyncClient,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        use crate::components::buttons::execute_command;
        use crate::components::switch::{execute_dbus_switch_command, execute_switch_command};
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
                    action,
                } => {
                    if topic == command_topic {
                        let payload = payload.trim();
                        if payload == "ON" || payload == "OFF" {
                            let switch_state = payload == "ON";
                            info!(
                                "Switch command received on topic '{}': {}, executing action",
                                topic, payload
                            );

                            let execution_result = match action {
                                SwitchAction::Exec(exec_command) => {
                                    execute_switch_command(exec_command, &payload.to_lowercase())
                                        .await
                                }
                                SwitchAction::DBus(dbus_action) => {
                                    execute_dbus_switch_command(dbus_action, switch_state).await
                                }
                            };

                            match execution_result {
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
                                    error!("Failed to execute switch command: {}", e);
                                    // Publish empty payload to indicate command failure
                                    client
                                        .publish(state_topic, QoS::AtLeastOnce, true, "")
                                        .await?;
                                    debug!(
                                        "Published empty state to topic '{}' due to command failure",
                                        state_topic
                                    );
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
                TopicHandler::Notification {
                    topic: notification_topic,
                } => {
                    if topic == notification_topic {
                        debug!(
                            "Processing notification command on topic '{}': {}",
                            topic, payload
                        );

                        // Use the notification handler from the notifications module
                        use crate::components::notifications::handle_notification_command;

                        match handle_notification_command(topic, payload, notification_topic).await
                        {
                            true => {
                                info!("Notification processed successfully");
                                return Ok(true);
                            }
                            false => {
                                // This shouldn't happen since we already matched the topic,
                                // but handle it gracefully
                                debug!("Notification handler returned false for matched topic");
                            }
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
                TopicHandler::Notification { topic, .. } => {
                    topics.push(topic.clone());
                }
            }
        }
        topics
    }
}
