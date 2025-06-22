use rumqttc::{AsyncClient, MqttOptions};
use std::time::Duration;
use tokio::time;
use tracing::{debug, info, warn};

use crate::components::{
    create_button_components_and_setup, create_notification_components_and_setup,
    create_switch_components_and_setup, create_system_sensor_components, SystemMonitor,
};
use crate::dbus::{create_status_component, StatusManager};
use crate::utils::Config;

use super::{publish_unified_discovery, TopicHandlers};

pub async fn initialize_mqtt_connection(
    config: &Config,
) -> Result<
    (
        AsyncClient,
        rumqttc::EventLoop,
        TopicHandlers,
        StatusManager,
        tokio::task::JoinHandle<()>,
    ),
    Box<dyn std::error::Error>,
> {
    // Set up MQTT options
    let mut mqttoptions = MqttOptions::new(&config.hostname, &config.mqtt_url, config.mqtt_port);
    mqttoptions.set_credentials(&config.username, &config.password);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    // Create MQTT client
    debug!("Creating MQTT client");
    let (client, eventloop) = AsyncClient::new(mqttoptions, 10);
    debug!("MQTT client created successfully");

    // Collect all components for unified discovery
    let mut all_components = Vec::new();
    let mut topic_handlers = TopicHandlers::new();

    // Handle button components and subscriptions
    let (button_components, button_topics) =
        create_button_components_and_setup(&client, config).await?;
    all_components.extend(button_components);

    // Add button topics to unified handlers
    for (topic, exec_command) in button_topics {
        topic_handlers.add_button(topic, exec_command);
    }

    // Handle switch components and subscriptions
    let (switch_components, switch_topics) =
        create_switch_components_and_setup(&client, config).await?;
    all_components.extend(switch_components);

    // Add switch topics to unified handlers
    for (command_topic, state_topic, exec_command) in switch_topics {
        topic_handlers.add_switch(command_topic, state_topic, exec_command);
    }

    // Handle notification components and subscriptions
    let (notification_components, notification_topic) =
        create_notification_components_and_setup(&client, config).await?;
    all_components.extend(notification_components);

    // Add notification topic to unified handlers
    topic_handlers.add_notification(notification_topic);

    // Create system monitoring sensor components
    let system_components = create_system_sensor_components(config);
    all_components.extend(system_components);

    // Create status sensor component
    let (status_id, status_component) = create_status_component(config);
    all_components.push((status_id, status_component));

    // Publish unified device discovery with all components
    info!(
        "Publishing unified device discovery with {} components",
        all_components.len()
    );
    publish_unified_discovery(&client, config, all_components).await?;

    info!("Discovery complete, briefly waiting...");
    time::sleep(Duration::from_millis(500)).await;

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
    let mut system_monitor = SystemMonitor::new(config.sensor_topic_base.clone(), client.clone());

    // Start system monitoring in background
    let monitoring_handle = tokio::spawn(async move {
        system_monitor.run_monitoring_loop().await;
    });

    Ok((
        client,
        eventloop,
        topic_handlers,
        status_manager,
        monitoring_handle,
    ))
}
