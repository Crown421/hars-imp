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
use crate::components::status::{StatusComponent, StatusValue};
use crate::components::system_monitor::SystemMonitorComponent;
use crate::components::trait_def::{ActionMessage, Component};
use crate::components::{button::ButtonComponent, switch::SwitchComponent};
use crate::config::Config;
use crate::dbus::power::{LogindDelayHold, PowerEvent, PowerMonitorSupervisor};
use crate::mqtt::client::{publish_retained, subscribe_topics, MqttClient, MqttEvent};
use crate::mqtt::discovery::DeviceDiscoveryBuilder;

/// The central coordinator that owns all components and runs the main event loop.
pub struct Orchestrator {
    config: Config,
}

struct MainLoopState<'a> {
    event_rx: &'a mut mpsc::Receiver<MqttEvent>,
    power_rx: mpsc::Receiver<PowerEvent>,
    mqtt_client: &'a rumqttc::AsyncClient,
    registry: &'a ComponentRegistry,
    action_tx: &'a mpsc::Sender<ActionMessage>,
    sync_tx: &'a mpsc::Sender<()>,
    polling_shutdown: &'a mut CancellationToken,
    polling_handles: &'a mut Vec<JoinHandle<()>>,
}

enum MainLoopExit {
    Signal,
    LogindShutdown(LogindDelayHold),
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
        let mut power_supervisor = PowerMonitorSupervisor::spawn();
        let power_rx = power_supervisor.take_receiver();
        let process_shutdown_hold = power_supervisor.shutdown_delay_hold();

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
        let exit = self
            .main_loop(MainLoopState {
                event_rx: &mut event_rx,
                power_rx,
                mqtt_client: &mqtt_async_client,
                registry: &registry,
                action_tx: &action_tx,
                sync_tx: &sync_tx,
                polling_shutdown: &mut polling_shutdown,
                polling_handles: &mut polling_handles,
            })
            .await?;

        // --- Graceful shutdown ---
        info!("Shutting down...");
        polling_shutdown.cancel();
        sync_shutdown.cancel();
        await_task("reconnect sync worker", sync_handle).await;
        let shutdown_hold = match exit {
            MainLoopExit::Signal => process_shutdown_hold,
            MainLoopExit::LogindShutdown(hold) => hold,
        };

        // Publish offline status.
        if let Err(e) = publish_retained(
            &mqtt_async_client,
            &StatusComponent::state_topic_for(&self.config.hostname),
            &StatusComponent::payload(StatusValue::Off),
        )
        .await
        {
            warn!("Failed to publish shutdown status sensor state: {e}");
        }

        if let Err(e) =
            publish_retained(&mqtt_async_client, &self.config.status_topic(), "offline").await
        {
            warn!("Failed to publish offline status: {e}");
        }

        // Give MQTT a moment to flush.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        shutdown_hold.release();
        power_supervisor.shutdown().await;

        info!("Shutdown complete");
        Ok(())
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

        // Built-in user-visible status sensor
        let status = Arc::new(StatusComponent::new(&self.config.hostname));
        info!("Registering status component");
        registry.register(status);

        // Built-in system monitor
        let system_monitor = Arc::new(SystemMonitorComponent::new(
            &self.config.hostname,
            self.config.update_interval_secs,
        ));
        info!("Registering system monitor");
        registry.register(system_monitor);
    }

    /// Build the HA device discovery JSON payload.
    fn build_discovery_json(
        &self,
        registry: &ComponentRegistry,
    ) -> Result<String, crate::error::AppError> {
        let mut seen_keys = HashSet::new();
        let mut components = Vec::new();

        for component in registry.components() {
            for (key, discovery_component) in component.discovery_components() {
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

                components.push((key, discovery_component));
            }
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
    async fn main_loop(
        &self,
        mut state: MainLoopState<'_>,
    ) -> Result<MainLoopExit, crate::error::AppError> {
        // Set up SIGTERM handler for systemd service deployments.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");

        loop {
            tokio::select! {
                // --- MQTT events ---
                Some(event) = state.event_rx.recv() => {
                    match event {
                        MqttEvent::Connected => {
                            info!("MQTT connected — scheduling reconnect synchronization");
                            request_reconnect_sync(state.sync_tx);
                        }
                        MqttEvent::Message(topic, payload) => {
                            state.registry.route_message(&topic, &payload, state.action_tx).await;
                        }
                        MqttEvent::Disconnected(reason) => {
                            warn!("MQTT disconnected: {reason}");
                        }
                    }
                }

                // --- Power events ---
                Some(power_event) = state.power_rx.recv() => {
                    match power_event {
                        PowerEvent::Suspending(hold) => {
                            info!("Handling suspend");
                            // Cancel polling tasks.
                            state.polling_shutdown.cancel();

                            if let Err(e) = publish_retained(
                                state.mqtt_client,
                                &StatusComponent::state_topic_for(&self.config.hostname),
                                &StatusComponent::payload(StatusValue::Suspended),
                            )
                            .await
                            {
                                warn!("Failed to publish suspended status sensor state: {e}");
                            }

                            if let Err(e) =
                                publish_retained(state.mqtt_client, &self.config.status_topic(), "offline").await
                            {
                                warn!("Failed to publish suspend availability status: {e}");
                            }

                            // Give MQTT time to flush.
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            hold.release();
                        }
                        PowerEvent::Resuming => {
                            info!("Handling resume");

                            // Create a fresh cancellation token — a child of a
                            // cancelled token is immediately cancelled, so we
                            // must replace the token entirely.
                            *state.polling_shutdown = CancellationToken::new();
                            *state.polling_handles = state.registry.spawn_polling_tasks(
                                state.action_tx.clone(),
                                state.polling_shutdown.clone(),
                            );

                            // Treat resume like a reconnect/state synchronization event
                            // even if the MQTT connection does not emit a fresh ConnAck.
                            request_reconnect_sync(state.sync_tx);
                        }
                        PowerEvent::ShuttingDown(hold) => {
                            info!("Received logind shutdown preparation signal");
                            return Ok(MainLoopExit::LogindShutdown(hold));
                        }
                    }
                }

                // --- Shutdown signals ---
                _ = signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down");
                    return Ok(MainLoopExit::Signal);
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, shutting down");
                    return Ok(MainLoopExit::Signal);
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

    // Publish user-visible status directly so lifecycle transitions stay ordered.
    publish_retained(
        client,
        &StatusComponent::state_topic_for(&config.hostname),
        &StatusComponent::payload(StatusValue::On),
    )
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
    use serde_json::Value;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn test_config() -> Config {
        Config {
            hostname: "testhost".to_string(),
            mqtt_url: "mqtt.example.com".to_string(),
            mqtt_port: None,
            username: "user".to_string(),
            password: "pass".to_string(),
            log_level: "info".to_string(),
            update_interval_secs: 60,
            button: vec![],
            switch: vec![],
            tls: None,
        }
    }

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

    #[tokio::test]
    async fn main_loop_exits_on_logind_shutdown_event() {
        let config = test_config();
        let orchestrator = Orchestrator::new(config);
        let registry = ComponentRegistry::new();
        let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(1);
        let (power_tx, power_rx) = mpsc::channel::<PowerEvent>(1);
        let (action_tx, _action_rx) = mpsc::channel::<ActionMessage>(1);
        let (sync_tx, _sync_rx) = mpsc::channel::<()>(1);
        let mut polling_shutdown = CancellationToken::new();
        let mut polling_handles = Vec::new();
        let mqtt_options = rumqttc::MqttOptions::new("test-main-loop", "localhost", 1883);
        let (mqtt_client, _eventloop) = rumqttc::AsyncClient::new(mqtt_options, 1);

        power_tx
            .send(PowerEvent::ShuttingDown(LogindDelayHold::empty_for_test(
                "shutdown",
            )))
            .await
            .expect("queue shutdown event");

        let exit = orchestrator
            .main_loop(MainLoopState {
                event_rx: &mut event_rx,
                power_rx,
                mqtt_client: &mqtt_client,
                registry: &registry,
                action_tx: &action_tx,
                sync_tx: &sync_tx,
                polling_shutdown: &mut polling_shutdown,
                polling_handles: &mut polling_handles,
            })
            .await
            .expect("main loop should exit cleanly");

        assert!(matches!(exit, MainLoopExit::LogindShutdown(_)));
        drop(event_tx);
    }

    #[test]
    fn discovery_json_includes_status_sensor() {
        let config = test_config();
        let orchestrator = Orchestrator::new(config.clone());
        let mut registry = ComponentRegistry::new();

        orchestrator.register_components(&mut registry);

        let json = orchestrator
            .build_discovery_json(&registry)
            .expect("build discovery json");
        let parsed: Value = serde_json::from_str(&json).expect("parse discovery");
        let status = &parsed["cmps"]["testhost_status"];

        assert_eq!(status["unique_id"], "testhost_status");
        assert_eq!(
            status["state_topic"],
            "homeassistant/sensor/testhost/status/state"
        );
        assert_eq!(status["value_template"], "{{ value_json.status }}");
    }
}
