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
use crate::dbus::power::{PowerEvent, PowerMonitorSupervisor};
use crate::mqtt::client::{
    subscribe_topics, MqttClient, MqttControlHandle, MqttEvent, TrackedPublishResult,
};
use crate::mqtt::discovery::DeviceDiscoveryBuilder;

const CRITICAL_PUBLISH_ACK_TIMEOUT: Duration = Duration::from_millis(500);
const RECONNECT_SYNC_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// The central coordinator that owns all components and runs the main event loop.
pub struct Orchestrator {
    config: Config,
}

struct MainLoopState<'a> {
    event_rx: &'a mut mpsc::Receiver<MqttEvent>,
    power_rx: mpsc::Receiver<PowerEvent>,
    mqtt_control: &'a MqttControlHandle,
    registry: &'a ComponentRegistry,
    action_tx: &'a mpsc::Sender<ActionMessage>,
    sync_tx: &'a mpsc::Sender<()>,
    polling_shutdown: &'a mut CancellationToken,
    polling_handles: &'a mut Vec<JoinHandle<()>>,
}

struct ReconnectSyncWorkerContext {
    config: Config,
    registry: Arc<ComponentRegistry>,
    client: rumqttc::AsyncClient,
    mqtt_control: MqttControlHandle,
    discovery_json: String,
    action_tx: mpsc::Sender<ActionMessage>,
}

struct PowerTopicPublish {
    topic: String,
    payload: String,
    timeout: Duration,
    error_description: &'static str,
    disconnect_log: &'static str,
    timeout_log: &'static str,
}

struct PowerTopicPublishFailure {
    publish: PowerTopicPublish,
    result: TrackedPublishResult,
}

#[derive(Debug)]
enum MainLoopExit {
    Signal { shutdown_already_published: bool },
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
        let mqtt_control = mqtt_client.control_handle();

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
            ReconnectSyncWorkerContext {
                config: self.config.clone(),
                registry: Arc::clone(&registry),
                client: mqtt_async_client.clone(),
                mqtt_control: mqtt_control.clone(),
                discovery_json: discovery_json.clone(),
                action_tx: action_tx.clone(),
            },
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
                mqtt_control: &mqtt_control,
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
        let MainLoopExit::Signal {
            shutdown_already_published,
        } = exit;

        if shutdown_already_published {
            process_shutdown_hold.release();
        } else {
            let _ = publish_shutdown_state(&self.config, &mqtt_control).await;
            process_shutdown_hold.release();
        }

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
            &self.config.ambient_light_monitor,
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

        let mut shutdown_already_published = false;

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

                            match publish_suspend_state(&self.config, state.mqtt_control).await {
                                TrackedPublishResult::Acked => {}
                                TrackedPublishResult::Disconnected => {
                                    warn!("Suspend state publish interrupted by MQTT disconnect");
                                }
                                TrackedPublishResult::TimedOut => {
                                    warn!("Timed out waiting for suspend state publish acknowledgment");
                                }
                            }
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

                            // Treat resume like the same reconnect/state synchronization
                            // flow used after initial startup or MQTT reconnect, even if
                            // the broker connection does not emit a fresh ConnAck.
                            request_reconnect_sync(state.sync_tx);
                        }
                        PowerEvent::ShuttingDown(hold) => {
                            info!("Received logind shutdown preparation signal");
                            let _ = publish_shutdown_state(&self.config, state.mqtt_control).await;
                            hold.release();
                            shutdown_already_published = true;
                        }
                        PowerEvent::ShutdownCancelled => {
                            info!("Received logind shutdown cancellation signal");
                            match publish_online_state(
                                &self.config,
                                state.mqtt_control,
                                CRITICAL_PUBLISH_ACK_TIMEOUT,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(e) => {
                                    warn!("Failed to publish shutdown cancellation status: {e}");
                                }
                            }
                            shutdown_already_published = false;
                            request_reconnect_sync(state.sync_tx);
                        }
                    }
                }

                // --- Shutdown signals ---
                _ = signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down");
                    return Ok(MainLoopExit::Signal {
                        shutdown_already_published,
                    });
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, shutting down");
                    return Ok(MainLoopExit::Signal {
                        shutdown_already_published,
                    });
                }
            }
        }
    }

    fn spawn_reconnect_sync_worker(
        context: ReconnectSyncWorkerContext,
        mut sync_rx: mpsc::Receiver<()>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let ReconnectSyncWorkerContext {
                config,
                registry,
                client,
                mqtt_control,
                discovery_json,
                action_tx,
            } = context;

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
                                &mqtt_control,
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
    mqtt_control: &MqttControlHandle,
    discovery_json: &str,
    action_tx: &mpsc::Sender<ActionMessage>,
) -> Result<(), crate::error::AppError> {
    require_tracked_publish(
        mqtt_control,
        &config.discovery_topic(),
        discovery_json,
        RECONNECT_SYNC_ACK_TIMEOUT,
        "discovery payload",
    )
    .await?;

    // Subscribe to all component topics.
    let topics = registry.all_subscriptions();
    subscribe_topics(client, &topics).await.map_err(Box::new)?;

    publish_online_state(config, mqtt_control, RECONNECT_SYNC_ACK_TIMEOUT).await?;

    // Publish current state for all stateful components as part of the shared
    // reconnect/startup and resume-driven synchronization flow.
    registry.sync_states(action_tx).await;
    Ok(())
}

async fn publish_online_state(
    config: &Config,
    mqtt_control: &MqttControlHandle,
    timeout: Duration,
) -> Result<(), crate::error::AppError> {
    publish_power_topics(mqtt_control, online_power_topic_publishes(config, timeout))
        .await
        .map_err(power_topic_publish_error)?;

    Ok(())
}

fn online_power_topic_publishes(config: &Config, timeout: Duration) -> [PowerTopicPublish; 2] {
    [
        // Overwrite the retained steady-state power value before making the
        // entity available again, otherwise HA can briefly replay Suspended/Off.
        PowerTopicPublish {
            topic: StatusComponent::state_topic_for(&config.hostname),
            payload: StatusComponent::payload(StatusValue::On),
            timeout,
            error_description: "status sensor state",
            disconnect_log: "",
            timeout_log: "",
        },
        PowerTopicPublish {
            topic: config.status_topic(),
            payload: "online".to_string(),
            timeout,
            error_description: "availability status",
            disconnect_log: "",
            timeout_log: "",
        },
    ]
}

async fn publish_suspend_state(
    config: &Config,
    mqtt_control: &MqttControlHandle,
) -> TrackedPublishResult {
    match publish_power_topics(
        mqtt_control,
        [
            PowerTopicPublish {
                topic: StatusComponent::state_topic_for(&config.hostname),
                payload: StatusComponent::payload(StatusValue::Suspended),
                timeout: CRITICAL_PUBLISH_ACK_TIMEOUT,
                error_description: "suspend status sensor state",
                disconnect_log: "",
                timeout_log: "",
            },
            PowerTopicPublish {
                topic: config.status_topic(),
                payload: "offline".to_string(),
                timeout: CRITICAL_PUBLISH_ACK_TIMEOUT,
                error_description: "suspend availability status",
                disconnect_log: "",
                timeout_log: "",
            },
        ],
    )
    .await
    {
        Ok(()) => TrackedPublishResult::Acked,
        Err(failure) => failure.result,
    }
}

async fn publish_shutdown_state(
    config: &Config,
    mqtt_control: &MqttControlHandle,
) -> TrackedPublishResult {
    match publish_power_topics(
        mqtt_control,
        [
            PowerTopicPublish {
                topic: StatusComponent::state_topic_for(&config.hostname),
                payload: StatusComponent::payload(StatusValue::Off),
                timeout: CRITICAL_PUBLISH_ACK_TIMEOUT,
                error_description: "shutdown status sensor state",
                disconnect_log: "Shutdown status publish interrupted by MQTT disconnect",
                timeout_log: "Timed out waiting for shutdown status sensor acknowledgment",
            },
            PowerTopicPublish {
                topic: config.status_topic(),
                payload: "offline".to_string(),
                timeout: CRITICAL_PUBLISH_ACK_TIMEOUT,
                error_description: "shutdown availability status",
                disconnect_log: "Shutdown availability publish interrupted by MQTT disconnect",
                timeout_log: "Timed out waiting for shutdown availability acknowledgment",
            },
        ],
    )
    .await
    {
        Ok(()) => TrackedPublishResult::Acked,
        Err(failure) => {
            match failure.result {
                TrackedPublishResult::Acked => {}
                TrackedPublishResult::Disconnected => warn!("{}", failure.publish.disconnect_log),
                TrackedPublishResult::TimedOut => warn!("{}", failure.publish.timeout_log),
            }
            failure.result
        }
    }
}

async fn publish_power_topics<I>(
    mqtt_control: &MqttControlHandle,
    publishes: I,
) -> Result<(), PowerTopicPublishFailure>
where
    I: IntoIterator<Item = PowerTopicPublish>,
{
    for publish in publishes {
        match mqtt_control
            .publish_retained_tracked(
                publish.topic.clone(),
                publish.payload.clone(),
                publish.timeout,
            )
            .await
        {
            TrackedPublishResult::Acked => {}
            result => return Err(PowerTopicPublishFailure { publish, result }),
        }
    }

    Ok(())
}

async fn require_tracked_publish(
    mqtt_control: &MqttControlHandle,
    topic: &str,
    payload: &str,
    timeout: Duration,
    description: &str,
) -> Result<(), crate::error::AppError> {
    match mqtt_control
        .publish_retained_tracked(topic.to_string(), payload.to_string(), timeout)
        .await
    {
        TrackedPublishResult::Acked => Ok(()),
        TrackedPublishResult::TimedOut => Err(Box::new(crate::error::MqttError::Internal(
            format!("Timed out waiting for broker acknowledgment for {description}"),
        ))
        .into()),
        TrackedPublishResult::Disconnected => Err(Box::new(crate::error::MqttError::Internal(
            format!("MQTT disconnected before broker acknowledgment for {description}"),
        ))
        .into()),
    }
}

fn power_topic_publish_error(failure: PowerTopicPublishFailure) -> crate::error::AppError {
    match failure.result {
        TrackedPublishResult::Acked => unreachable!("acked publishes do not fail"),
        TrackedPublishResult::TimedOut => Box::new(crate::error::MqttError::Internal(format!(
            "Timed out waiting for broker acknowledgment for {}",
            failure.publish.error_description
        )))
        .into(),
        TrackedPublishResult::Disconnected => Box::new(crate::error::MqttError::Internal(format!(
            "MQTT disconnected before broker acknowledgment for {}",
            failure.publish.error_description
        )))
        .into(),
    }
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
            ambient_light_monitor: crate::config::AmbientLightMonitorConfig::default(),
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
    async fn main_loop_does_not_exit_on_logind_shutdown_event() {
        let config = test_config();
        let orchestrator = Orchestrator::new(config);
        let registry = ComponentRegistry::new();
        let (_event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(1);
        let (power_tx, power_rx) = mpsc::channel::<PowerEvent>(1);
        let (action_tx, _action_rx) = mpsc::channel::<ActionMessage>(1);
        let (sync_tx, _sync_rx) = mpsc::channel::<()>(1);
        let mut polling_shutdown = CancellationToken::new();
        let mut polling_handles = Vec::new();
        let mqtt_client = MqttClient::new(&test_config()).expect("create mqtt client");
        let mqtt_control = mqtt_client.control_handle();
        drop(mqtt_client);

        power_tx
            .send(PowerEvent::ShuttingDown(
                crate::dbus::power::LogindDelayHold::empty_for_test("shutdown"),
            ))
            .await
            .expect("queue shutdown event");

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            orchestrator.main_loop(MainLoopState {
                event_rx: &mut event_rx,
                power_rx,
                mqtt_control: &mqtt_control,
                registry: &registry,
                action_tx: &action_tx,
                sync_tx: &sync_tx,
                polling_shutdown: &mut polling_shutdown,
                polling_handles: &mut polling_handles,
            }),
        )
        .await;

        assert!(
            result.is_err(),
            "main loop should stay running after shutdown prepare"
        );
    }

    #[tokio::test]
    async fn shutdown_cancelled_requests_reconnect_sync() {
        let config = test_config();
        let orchestrator = Orchestrator::new(config);
        let registry = ComponentRegistry::new();
        let (_event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(1);
        let (power_tx, power_rx) = mpsc::channel::<PowerEvent>(1);
        let (action_tx, _action_rx) = mpsc::channel::<ActionMessage>(1);
        let (sync_tx, mut sync_rx) = mpsc::channel::<()>(1);
        let mut polling_shutdown = CancellationToken::new();
        let mut polling_handles = Vec::new();
        let mqtt_client = MqttClient::new(&test_config()).expect("create mqtt client");
        let mqtt_control = mqtt_client.control_handle();
        drop(mqtt_client);

        power_tx
            .send(PowerEvent::ShutdownCancelled)
            .await
            .expect("queue shutdown cancellation event");

        let main_loop = orchestrator.main_loop(MainLoopState {
            event_rx: &mut event_rx,
            power_rx,
            mqtt_control: &mqtt_control,
            registry: &registry,
            action_tx: &action_tx,
            sync_tx: &sync_tx,
            polling_shutdown: &mut polling_shutdown,
            polling_handles: &mut polling_handles,
        });

        tokio::select! {
            result = main_loop => panic!("main loop should not exit on shutdown cancellation: {result:?}"),
            sync = sync_rx.recv() => assert!(sync.is_some(), "shutdown cancellation should request reconnect sync"),
        }
    }

    #[test]
    fn online_power_topics_publish_status_before_availability() {
        let config = test_config();
        let publishes = online_power_topic_publishes(&config, Duration::from_secs(2));

        assert_eq!(
            publishes[0].topic,
            StatusComponent::state_topic_for(&config.hostname)
        );
        assert_eq!(
            publishes[0].payload,
            StatusComponent::payload(StatusValue::On)
        );
        assert_eq!(publishes[1].topic, config.status_topic());
        assert_eq!(publishes[1].payload, "online");
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
