use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::components::notification::NotificationComponent;
use crate::components::registry::ComponentRegistry;
use crate::components::system_monitor::{CpuSensor, DiskUsageSensor, MemorySensor};
use crate::components::trait_def::{ActionMessage, Component, OutboundMessage};
use crate::components::{button::ButtonComponent, switch::SwitchComponent};
use crate::config::Config;
use crate::dbus::power::{PowerEvent, PowerMonitorSupervisor};
use crate::mqtt::client::{publish_retained, subscribe_topics, MqttClient, MqttEvent};
use crate::mqtt::discovery::DeviceDiscoveryBuilder;
use crate::util::helpers::slugify;

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
        let registry = Arc::new(registry);

        info!(
            "Registered {} components with {} subscriptions",
            registry.components().len(),
            registry.all_subscriptions().len()
        );

        // --- Set up channels ---
        let (action_tx, action_rx) = mpsc::channel::<ActionMessage>(100);
        let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
        let (sync_tx, sync_rx) = mpsc::channel::<()>(8);

        // --- Create MQTT client ---
        let mqtt_client = MqttClient::new(&self.config)?;
        let mqtt_async_client = mqtt_client.client();

        // --- Set up supervised D-Bus power monitoring ---
        let power_supervisor = PowerMonitorSupervisor::spawn();
        let power_rx = power_supervisor.receiver.resubscribe();

        // --- Prepare discovery payload ---
        let discovery_json = self.build_discovery_json(&registry)?;

        // --- Spawn MQTT event loop ---
        let _mqtt_handle = tokio::spawn(mqtt_client.run(event_tx, action_rx));

        // --- Spawn reconnect synchronization worker ---
        let sync_shutdown = CancellationToken::new();
        let sync_handle = Self::spawn_reconnect_sync_worker(
            self.config.clone(),
            Arc::clone(&registry),
            mqtt_async_client.clone(),
            discovery_json.clone(),
            action_tx.clone(),
            sync_rx,
            sync_shutdown.clone(),
        );

        // --- Cancellation token for polling tasks ---
        let mut polling_shutdown = CancellationToken::new();
        let mut polling_handles =
            registry.spawn_polling_tasks(action_tx.clone(), polling_shutdown.clone());

        // --- Main event loop ---
        info!("Entering main event loop");
        let result = self
            .main_loop(
                &mut event_rx,
                power_rx,
                &registry,
                &action_tx,
                &sync_tx,
                &mut polling_shutdown,
                &mut polling_handles,
            )
            .await;

        // --- Graceful shutdown ---
        info!("Shutting down...");
        polling_shutdown.cancel();
        sync_shutdown.cancel();
        await_task("reconnect sync worker", sync_handle).await;
        power_supervisor.shutdown().await;

        // Publish offline status.
        if let Err(e) =
            publish_retained(&mqtt_async_client, &self.config.status_topic(), "offline").await
        {
            warn!("Failed to publish offline status: {e}");
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

        // Built-in disk usage sensor (root partition)
        let disk = Arc::new(DiskUsageSensor::new(
            &self.config.hostname,
            self.config.update_interval_secs,
            None, // defaults to "/"
        ));
        info!("Registering disk usage sensor");
        registry.register(disk);
    }

    /// Build the HA device discovery JSON payload.
    fn build_discovery_json(
        &self,
        registry: &ComponentRegistry,
    ) -> Result<String, crate::error::AppError> {
        let mut seen_keys = HashSet::new();
        let mut components = Vec::new();

        for component in registry.components() {
            let key = slugify(component.name());
            if key.is_empty() {
                return Err(crate::error::ComponentError::DiscoveryConflict(format!(
                    "component '{}' produced an empty discovery key",
                    component.name()
                ))
                .into());
            }

            if !seen_keys.insert(key.clone()) {
                return Err(crate::error::ComponentError::DiscoveryConflict(format!(
                    "duplicate discovery key '{key}' from component '{}'",
                    component.name()
                ))
                .into());
            }

            components.push((key, component.discovery_component()));
        }

        let discovery = DeviceDiscoveryBuilder::new(&self.config)
            .add_components(components)
            .with_status_topic(self.config.status_topic())
            .build();

        let json = serde_json::to_string(&discovery)
            .map_err(crate::error::MqttError::Serialization)
            .map_err(Box::new)?;

        Ok(json)
    }

    /// The main event loop: select! over MQTT events, power events, and shutdown signals.
    #[allow(clippy::too_many_arguments)]
    async fn main_loop(
        &self,
        event_rx: &mut mpsc::Receiver<MqttEvent>,
        mut power_rx: tokio::sync::broadcast::Receiver<PowerEvent>,
        registry: &ComponentRegistry,
        action_tx: &mpsc::Sender<ActionMessage>,
        sync_tx: &mpsc::Sender<()>,
        polling_shutdown: &mut CancellationToken,
        polling_handles: &mut Vec<JoinHandle<()>>,
    ) -> Result<(), crate::error::AppError> {
        // Set up SIGTERM handler for systemd service deployments.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");

        loop {
            tokio::select! {
                // --- MQTT events ---
                Some(event) = event_rx.recv() => {
                    match event {
                        MqttEvent::Connected => {
                            info!("MQTT connected — scheduling reconnect synchronization");
                            request_reconnect_sync(sync_tx);
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
                                .send(OutboundMessage::availability(
                                    self.config.status_topic(),
                                    "offline",
                                ))
                                .await;

                            // Give MQTT time to flush.
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                        PowerEvent::Resuming => {
                            info!("Handling resume");

                            // Create a fresh cancellation token — a child of a
                            // cancelled token is immediately cancelled, so we
                            // must replace the token entirely.
                            *polling_shutdown = CancellationToken::new();
                            *polling_handles = registry.spawn_polling_tasks(
                                action_tx.clone(),
                                polling_shutdown.clone(),
                            );

                            // Treat resume like a reconnect/state synchronization event
                            // even if the MQTT connection does not emit a fresh ConnAck.
                            request_reconnect_sync(sync_tx);
                        }
                    }
                }

                // --- Shutdown signals ---
                _ = signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down");
                    return Ok(());
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, shutting down");
                    return Ok(());
                }
            }
        }
    }

    fn spawn_reconnect_sync_worker(
        config: Config,
        registry: Arc<ComponentRegistry>,
        client: rumqttc::AsyncClient,
        discovery_json: String,
        action_tx: mpsc::Sender<ActionMessage>,
        mut sync_rx: mpsc::Receiver<()>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("Reconnect sync worker shutting down");
                        return;
                    }
                    Some(()) = sync_rx.recv() => {
                        drain_reconnect_sync_requests(&mut sync_rx);
                        retry_reconnect_sync(&shutdown, Duration::from_secs(1), Duration::from_secs(30), || {
                            reconnect_sync_once(
                                &config,
                                registry.as_ref(),
                                &client,
                                &discovery_json,
                                &action_tx,
                            )
                        }).await;
                    }
                    else => return,
                }
            }
        })
    }
}

async fn reconnect_sync_once(
    config: &Config,
    registry: &ComponentRegistry,
    client: &rumqttc::AsyncClient,
    discovery_json: &str,
    action_tx: &mpsc::Sender<ActionMessage>,
) -> Result<(), crate::error::AppError> {
    // Publish discovery (retained).
    publish_retained(client, &config.discovery_topic(), discovery_json)
        .await
        .map_err(Box::new)?;

    // Subscribe to all component topics.
    let topics = registry.all_subscriptions();
    subscribe_topics(client, &topics).await.map_err(Box::new)?;

    // Publish online status (retained).
    publish_retained(client, &config.status_topic(), "online")
        .await
        .map_err(Box::new)?;

    // Publish current state for all stateful components (e.g. switches).
    registry.notify_resume(action_tx).await;
    Ok(())
}

async fn retry_reconnect_sync<F, Fut>(
    shutdown: &CancellationToken,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut sync_once: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::error::AppError>>,
{
    let mut attempt: u64 = 1;
    let mut backoff = initial_backoff;

    loop {
        match sync_once().await {
            Ok(()) => {
                info!("Reconnect synchronization completed");
                return;
            }
            Err(e) => {
                warn!("Reconnect synchronization attempt {attempt} failed: {e}; retrying in {backoff:?}");
            }
        }

        let delay = backoff;
        attempt += 1;
        backoff = (backoff * 2).min(max_backoff);

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn request_reconnect_sync(sync_tx: &mpsc::Sender<()>) {
    match sync_tx.try_send(()) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("Reconnect synchronization already queued")
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!("Reconnect synchronization worker is not running")
        }
    }
}

fn drain_reconnect_sync_requests(sync_rx: &mut mpsc::Receiver<()>) {
    while sync_rx.try_recv().is_ok() {}
}

async fn await_task(name: &str, handle: JoinHandle<()>) {
    match tokio::time::timeout(Duration::from_secs(2), handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("{name} failed during shutdown: {e}"),
        Err(_) => warn!("Timed out waiting for {name} shutdown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn reconnect_sync_retries_until_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_sync = Arc::clone(&attempts);
        let shutdown = CancellationToken::new();

        retry_reconnect_sync(
            &shutdown,
            Duration::from_millis(1),
            Duration::from_millis(2),
            move || {
                let attempts = Arc::clone(&attempts_for_sync);
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        Err(crate::error::ComponentError::InvalidPayload("fail once".into()).into())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
