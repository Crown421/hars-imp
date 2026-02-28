use std::sync::Arc;

use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::components::button::slugify;
use crate::components::notification::NotificationComponent;
use crate::components::registry::ComponentRegistry;
use crate::components::system_monitor::{CpuSensor, MemorySensor};
use crate::components::trait_def::{ActionMessage, Component};
use crate::components::{button::ButtonComponent, switch::SwitchComponent};
use crate::config::Config;
use crate::dbus::power::{PowerEvent, PowerMonitor};
use crate::mqtt::client::{MqttClient, MqttEvent, publish_retained, subscribe_topics};
use crate::mqtt::discovery::DeviceDiscoveryBuilder;

/// The central coordinator that owns all components and runs the main event loop.
pub struct Orchestrator {
    config: Config,
}

impl Orchestrator {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the application: build components, connect, and enter the main loop.
    pub async fn run(self) -> Result<(), crate::error::AppError> {
        // --- Build component registry ---
        let mut registry = ComponentRegistry::new();
        self.register_components(&mut registry);

        info!(
            "Registered {} components with {} subscriptions",
            registry.components().len(),
            registry.all_subscriptions().len()
        );

        // --- Set up channels ---
        let (action_tx, action_rx) = mpsc::channel::<ActionMessage>(100);
        let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);

        // --- Create MQTT client ---
        let mqtt_client = MqttClient::new(&self.config);
        let mqtt_async_client = mqtt_client.client();

        // --- Set up D-Bus power monitoring ---
        let power_rx = self.setup_power_monitor().await;

        // --- Prepare discovery payload ---
        let discovery_json = self.build_discovery_json(&registry)?;

        // --- Spawn MQTT event loop ---
        let _mqtt_handle = tokio::spawn(mqtt_client.run(event_tx, action_rx));

        // --- Cancellation token for polling tasks ---
        let polling_shutdown = CancellationToken::new();
        let mut polling_handles = registry.spawn_polling_tasks(
            action_tx.clone(),
            polling_shutdown.clone(),
        );

        // --- Main event loop ---
        info!("Entering main event loop");
        let result = self
            .main_loop(
                &mut event_rx,
                power_rx,
                &registry,
                &action_tx,
                &mqtt_async_client,
                &discovery_json,
                &polling_shutdown,
                &mut polling_handles,
            )
            .await;

        // --- Graceful shutdown ---
        info!("Shutting down...");
        polling_shutdown.cancel();

        // Publish offline status.
        if let Err(e) = publish_retained(
            &mqtt_async_client,
            &self.config.status_topic(),
            "offline",
        )
        .await
        {
            warn!("Failed to publish offline status: {e}");
        }

        // Publish empty discovery to remove device from HA.
        if let Err(e) = publish_retained(
            &mqtt_async_client,
            &self.config.discovery_topic(),
            "",
        )
        .await
        {
            warn!("Failed to clear discovery: {e}");
        }

        // Give MQTT a moment to flush.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        info!("Shutdown complete");
        result
    }

    /// Register all components from config + built-ins.
    fn register_components(&self, registry: &mut ComponentRegistry) {
        // Buttons from config
        for btn_config in &self.config.button {
            let component = Arc::new(ButtonComponent::new(btn_config, &self.config.hostname));
            info!("Registering button: {}", component.name());
            registry.register(component);
        }

        // Switches from config
        for sw_config in &self.config.switch {
            let component = Arc::new(SwitchComponent::new(sw_config, &self.config.hostname));
            info!("Registering switch: {}", component.name());
            registry.register(component);
        }

        // Built-in notification component
        let notification = Arc::new(NotificationComponent::new(&self.config.hostname));
        info!("Registering notification component");
        registry.register(notification);

        // Built-in system sensors
        let cpu = Arc::new(CpuSensor::new(
            &self.config.hostname,
            self.config.update_interval_secs,
        ));
        let memory = Arc::new(MemorySensor::new(
            &self.config.hostname,
            self.config.update_interval_secs,
        ));
        info!("Registering CPU and memory sensors");
        registry.register(cpu);
        registry.register(memory);
    }

    /// Build the HA device discovery JSON payload.
    fn build_discovery_json(
        &self,
        registry: &ComponentRegistry,
    ) -> Result<String, crate::error::AppError> {
        let components = registry
            .components()
            .iter()
            .map(|c| {
                let key = slugify(c.name());
                let discovery = c.discovery_component();
                (key, discovery)
            })
            .collect::<Vec<_>>();

        let discovery = DeviceDiscoveryBuilder::new(&self.config)
            .add_components(components)
            .with_status_topic(self.config.status_topic())
            .build();

        let json = serde_json::to_string(&discovery)
            .map_err(crate::error::MqttError::Serialization)?;

        Ok(json)
    }

    /// Set up the D-Bus power monitor. Returns a broadcast receiver.
    ///
    /// If D-Bus is unavailable (e.g. in a container), logs a warning and
    /// returns a receiver that will never produce events.
    async fn setup_power_monitor(
        &self,
    ) -> tokio::sync::broadcast::Receiver<PowerEvent> {
        match crate::dbus::client::system_connection().await {
            Ok(conn) => match PowerMonitor::new(conn).await {
                Ok((monitor, rx)) => {
                    tokio::spawn(async move {
                        if let Err(e) = monitor.run().await {
                            error!("Power monitor error: {e}");
                        }
                    });
                    rx
                }
                Err(e) => {
                    warn!("Failed to create power monitor: {e}");
                    let (tx, rx) = tokio::sync::broadcast::channel(1);
                    std::mem::forget(tx); // keep alive
                    rx
                }
            },
            Err(e) => {
                warn!("D-Bus system bus unavailable: {e}. Power monitoring disabled.");
                let (tx, rx) = tokio::sync::broadcast::channel(1);
                std::mem::forget(tx);
                rx
            }
        }
    }

    /// The main event loop: select! over MQTT events, power events, and shutdown signals.
    #[allow(clippy::too_many_arguments)]
    async fn main_loop(
        &self,
        event_rx: &mut mpsc::Receiver<MqttEvent>,
        mut power_rx: tokio::sync::broadcast::Receiver<PowerEvent>,
        registry: &ComponentRegistry,
        action_tx: &mpsc::Sender<ActionMessage>,
        mqtt_client: &rumqttc::AsyncClient,
        discovery_json: &str,
        polling_shutdown: &CancellationToken,
        polling_handles: &mut Vec<JoinHandle<()>>,
    ) -> Result<(), crate::error::AppError> {
        loop {
            tokio::select! {
                // --- MQTT events ---
                Some(event) = event_rx.recv() => {
                    match event {
                        MqttEvent::Connected => {
                            info!("MQTT connected — publishing discovery and subscribing");
                            self.on_mqtt_connected(
                                mqtt_client,
                                registry,
                                discovery_json,
                            ).await;
                        }
                        MqttEvent::Message(topic, payload) => {
                            registry.route_message(&topic, &payload, action_tx).await;
                        }
                        MqttEvent::Disconnected(reason) => {
                            warn!("MQTT disconnected: {reason}");
                        }
                    }
                }

                // --- Power events ---
                Ok(power_event) = power_rx.recv() => {
                    match power_event {
                        PowerEvent::Suspending => {
                            info!("Handling suspend");
                            // Cancel polling tasks.
                            polling_shutdown.cancel();

                            // Publish suspended status.
                            let _ = action_tx
                                .send((self.config.status_topic(), "offline".to_string()))
                                .await;

                            // Give MQTT time to flush.
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                        PowerEvent::Resuming => {
                            info!("Handling resume");

                            // Restart polling tasks with a new token.
                            let new_shutdown = polling_shutdown.child_token();
                            *polling_handles = registry.spawn_polling_tasks(
                                action_tx.clone(),
                                new_shutdown,
                            );

                            // Notify all components to re-publish state.
                            registry.notify_resume(action_tx).await;
                        }
                    }
                }

                // --- Shutdown signals ---
                _ = signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down");
                    return Ok(());
                }
            }
        }
    }

    /// Called when MQTT connects/reconnects: publish discovery, subscribe, publish online.
    async fn on_mqtt_connected(
        &self,
        client: &rumqttc::AsyncClient,
        registry: &ComponentRegistry,
        discovery_json: &str,
    ) {
        // Publish discovery (retained).
        if let Err(e) =
            publish_retained(client, &self.config.discovery_topic(), discovery_json).await
        {
            error!("Failed to publish discovery: {e}");
        }

        // Subscribe to all component topics.
        let topics = registry.all_subscriptions();
        if let Err(e) = subscribe_topics(client, &topics).await {
            error!("Failed to subscribe: {e}");
        }

        // Publish online status (retained).
        if let Err(e) =
            publish_retained(client, &self.config.status_topic(), "online").await
        {
            error!("Failed to publish online status: {e}");
        }
    }
}
