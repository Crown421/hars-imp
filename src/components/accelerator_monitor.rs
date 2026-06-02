use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::accelerator::{
    start_collector, AcceleratorCollector, AcceleratorDeviceSnapshot, AcceleratorKind,
    AcceleratorMetricReading, AcceleratorSnapshot,
};
use crate::components::trait_def::{ActionMessage, OutboundMessage};
use crate::config::Config;
use crate::mqtt::discovery::{ComponentType, DiscoveryCatalog, HomeAssistantComponent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceleratorMetricKind {
    Utilization,
    MemoryTotal,
    MemoryUsed,
    Temperature,
    Power,
}

#[derive(Debug, Clone, Copy)]
struct AcceleratorMetricSpec {
    kind: AcceleratorMetricKind,
    suffix: &'static str,
    display_name: &'static str,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
}

const ACCELERATOR_METRICS: [AcceleratorMetricSpec; 5] = [
    AcceleratorMetricSpec {
        kind: AcceleratorMetricKind::Utilization,
        suffix: "utilization",
        display_name: "Utilization",
        unit: Some("%"),
        device_class: None,
    },
    AcceleratorMetricSpec {
        kind: AcceleratorMetricKind::MemoryTotal,
        suffix: "memory_total",
        display_name: "Memory Total",
        unit: Some("MB"),
        device_class: Some("data_size"),
    },
    AcceleratorMetricSpec {
        kind: AcceleratorMetricKind::MemoryUsed,
        suffix: "memory_used",
        display_name: "Memory Used",
        unit: Some("MB"),
        device_class: Some("data_size"),
    },
    AcceleratorMetricSpec {
        kind: AcceleratorMetricKind::Temperature,
        suffix: "temperature",
        display_name: "Temperature",
        unit: Some("\u{b0}C"),
        device_class: Some("temperature"),
    },
    AcceleratorMetricSpec {
        kind: AcceleratorMetricKind::Power,
        suffix: "power",
        display_name: "Power",
        unit: Some("W"),
        device_class: Some("power"),
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupMetricSource {
    Value,
    ReadError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InactiveState {
    CollectionError(String),
    NoSupportedMetrics,
}

enum CollectStep {
    Sample(Result<AcceleratorSnapshot, String>),
    Pause,
    Shutdown,
}

enum LoopDirective {
    Continue,
    Pause,
    Shutdown,
    Stop,
}

struct SampleContext<'a> {
    config: &'a Config,
    discovery_topic: &'a str,
    state_topic: &'a str,
    discovery_catalog: &'a DiscoveryCatalog,
    action_tx: &'a mpsc::Sender<ActionMessage>,
    pause_rx: &'a mut watch::Receiver<bool>,
    shutdown: &'a CancellationToken,
}

pub struct AcceleratorMonitorService {
    pause_tx: watch::Sender<bool>,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

struct ActiveMonitorState {
    discovery_entries: Vec<(String, HomeAssistantComponent)>,
    advertised_metrics: HashMap<String, StartupMetricSource>,
    logged_not_supported_transitions: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone)]
struct ProbeDescriptor {
    key: String,
    name: String,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
    startup_source: StartupMetricSource,
}

impl AcceleratorMonitorService {
    pub fn spawn(
        config: &Config,
        discovery_catalog: Arc<DiscoveryCatalog>,
        action_tx: mpsc::Sender<ActionMessage>,
    ) -> Option<Self> {
        if !config.accelerator_monitor.enabled {
            return None;
        }

        let collector = start_collector(config.accelerator_monitor.clone());
        Some(Self::spawn_with_collector(
            config,
            discovery_catalog,
            action_tx,
            collector,
        ))
    }

    pub(crate) fn spawn_with_collector(
        config: &Config,
        discovery_catalog: Arc<DiscoveryCatalog>,
        action_tx: mpsc::Sender<ActionMessage>,
        collector: Arc<dyn AcceleratorCollector>,
    ) -> Self {
        let (pause_tx, pause_rx) = watch::channel(false);
        let shutdown = CancellationToken::new();
        let config = config.clone();
        let shutdown_for_task = shutdown.clone();
        let handle = tokio::spawn(async move {
            run_monitor_loop(
                config,
                discovery_catalog,
                action_tx,
                collector,
                pause_rx,
                shutdown_for_task,
            )
            .await;
        });

        Self {
            pause_tx,
            shutdown,
            handle,
        }
    }

    pub fn pause(&self) {
        let _ = self.pause_tx.send(true);
    }

    pub fn resume(&self) {
        let _ = self.pause_tx.send(false);
    }

    pub fn shutdown(self) -> JoinHandle<()> {
        self.shutdown.cancel();
        self.handle
    }
}

impl ActiveMonitorState {
    fn from_probe(hostname: &str, state_topic: &str, probe: &AcceleratorSnapshot) -> Option<Self> {
        let descriptors = metric_descriptors(probe);
        if descriptors.is_empty() {
            return None;
        }

        let discovery_entries = descriptors
            .iter()
            .map(|descriptor| {
                (
                    format!("{hostname}_{}", descriptor.key),
                    HomeAssistantComponent {
                        name: descriptor.name.clone(),
                        unique_id: format!("{hostname}_{}", descriptor.key),
                        component_type: ComponentType::Sensor {
                            state_topic: state_topic.to_string(),
                            device_class: descriptor.device_class.map(str::to_string),
                            unit_of_measurement: descriptor.unit.map(str::to_string),
                            value_template: Some(format!(
                                "{{{{ value_json.{} }}}}",
                                descriptor.key
                            )),
                            icon: None,
                        },
                    },
                )
            })
            .collect::<Vec<_>>();
        let advertised_metrics = descriptors
            .into_iter()
            .map(|descriptor| (descriptor.key, descriptor.startup_source))
            .collect();

        Some(Self {
            discovery_entries,
            advertised_metrics,
            logged_not_supported_transitions: Mutex::new(HashSet::new()),
        })
    }

    fn payload_from_snapshot(&self, snapshot: &AcceleratorSnapshot) -> BTreeMap<String, f32> {
        let mut payload = BTreeMap::new();

        for device in &snapshot.devices {
            for spec in ACCELERATOR_METRICS {
                let key = metric_key(device, spec);
                let Some(startup_source) = self.advertised_metrics.get(&key).copied() else {
                    continue;
                };

                match metric_reading(device, spec) {
                    AcceleratorMetricReading::Value(value) => {
                        payload.insert(key, round_to_2dp(*value));
                    }
                    AcceleratorMetricReading::NotSupported => {
                        if startup_source == StartupMetricSource::ReadError {
                            self.log_not_supported_transition_once(&key, device, spec);
                        }
                    }
                    AcceleratorMetricReading::ReadError(err) => {
                        debug!("Skipping accelerator metric '{key}': {err}");
                    }
                }
            }
        }

        payload
    }

    fn log_not_supported_transition_once(
        &self,
        key: &str,
        device: &AcceleratorDeviceSnapshot,
        spec: AcceleratorMetricSpec,
    ) {
        let mut logged = self
            .logged_not_supported_transitions
            .lock()
            .expect("transition log mutex should not be poisoned");
        if logged.insert(key.to_string()) {
            warn!(
                "Accelerator metric '{}' for {} is not supported after startup probing",
                spec.suffix,
                device_label(device)
            );
        }
    }
}

async fn run_monitor_loop(
    config: Config,
    discovery_catalog: Arc<DiscoveryCatalog>,
    action_tx: mpsc::Sender<ActionMessage>,
    collector: Arc<dyn AcceleratorCollector>,
    mut pause_rx: watch::Receiver<bool>,
    shutdown: CancellationToken,
) {
    let state_topic = format!(
        "homeassistant/sensor/{}/accelerator_performance/state",
        config.hostname
    );
    let discovery_topic = config.discovery_topic();
    let interval = Duration::from_secs(config.update_interval_secs);
    let mut ticker = delayed_ticker(interval);

    let mut active_state: Option<ActiveMonitorState> = None;
    let mut inactive_state: Option<InactiveState> = None;
    let mut paused = *pause_rx.borrow();
    let mut run_sample_now = true;

    loop {
        if paused {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    collector.shutdown();
                    debug!("Accelerator monitor service shutting down");
                    return;
                }
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        collector.shutdown();
                        debug!("Accelerator monitor service shutting down");
                        return;
                    }
                    let was_paused = paused;
                    paused = *pause_rx.borrow();
                    if was_paused && !paused {
                        run_sample_now = true;
                        ticker = delayed_ticker(interval);
                    }
                }
            }
            continue;
        }

        if run_sample_now {
            run_sample_now = false;
            let step = collect_or_control(Arc::clone(&collector), &mut pause_rx, &shutdown).await;
            let mut sample_context = SampleContext {
                config: &config,
                discovery_topic: &discovery_topic,
                state_topic: &state_topic,
                discovery_catalog: discovery_catalog.as_ref(),
                action_tx: &action_tx,
                pause_rx: &mut pause_rx,
                shutdown: &shutdown,
            };
            if apply_loop_directive(
                handle_collect_step(
                    step,
                    &mut sample_context,
                    &mut active_state,
                    &mut inactive_state,
                )
                .await,
                &collector,
                &mut paused,
            ) {
                return;
            }
            continue;
        }

        tokio::select! {
            _ = shutdown.cancelled() => {
                collector.shutdown();
                debug!("Accelerator monitor service shutting down");
                return;
            }
            changed = pause_rx.changed() => {
                if changed.is_err() {
                    collector.shutdown();
                    debug!("Accelerator monitor service shutting down");
                    return;
                }
                let was_paused = paused;
                paused = *pause_rx.borrow();
                if was_paused && !paused {
                    run_sample_now = true;
                    ticker = delayed_ticker(interval);
                }
            }
            _ = ticker.tick() => {
                let step = collect_or_control(Arc::clone(&collector), &mut pause_rx, &shutdown).await;
                let mut sample_context = SampleContext {
                    config: &config,
                    discovery_topic: &discovery_topic,
                    state_topic: &state_topic,
                    discovery_catalog: discovery_catalog.as_ref(),
                    action_tx: &action_tx,
                    pause_rx: &mut pause_rx,
                    shutdown: &shutdown,
                };
                if apply_loop_directive(
                    handle_collect_step(
                        step,
                        &mut sample_context,
                        &mut active_state,
                        &mut inactive_state,
                    )
                    .await,
                    &collector,
                    &mut paused,
                ) {
                    return;
                }
            }
        }
    }
}

async fn handle_collect_step(
    step: CollectStep,
    context: &mut SampleContext<'_>,
    active_state: &mut Option<ActiveMonitorState>,
    inactive_state: &mut Option<InactiveState>,
) -> LoopDirective {
    match step {
        CollectStep::Sample(sample) => {
            process_sample(context, active_state, inactive_state, sample).await
        }
        CollectStep::Pause => LoopDirective::Pause,
        CollectStep::Shutdown => LoopDirective::Shutdown,
    }
}

fn apply_loop_directive(
    directive: LoopDirective,
    collector: &Arc<dyn AcceleratorCollector>,
    paused: &mut bool,
) -> bool {
    match directive {
        LoopDirective::Continue => false,
        LoopDirective::Pause => {
            *paused = true;
            false
        }
        LoopDirective::Shutdown | LoopDirective::Stop => {
            collector.shutdown();
            debug!("Accelerator monitor service shutting down");
            true
        }
    }
}

async fn process_sample(
    context: &mut SampleContext<'_>,
    active_state: &mut Option<ActiveMonitorState>,
    inactive_state: &mut Option<InactiveState>,
    sample: Result<AcceleratorSnapshot, String>,
) -> LoopDirective {
    match active_state {
        Some(state) => match sample {
            Ok(snapshot) => {
                if let Some(directive) =
                    pending_control_directive(context.pause_rx, context.shutdown)
                {
                    return directive;
                }
                publish_snapshot_state(
                    context.action_tx,
                    context.pause_rx,
                    context.shutdown,
                    context.state_topic,
                    &snapshot,
                    state,
                )
                .await
            }
            Err(err) => {
                warn!("Skipping accelerator sample: {err}");
                LoopDirective::Continue
            }
        },
        None => match sample {
            Ok(snapshot) => {
                let Some(state) = ActiveMonitorState::from_probe(
                    &context.config.hostname,
                    context.state_topic,
                    &snapshot,
                ) else {
                    log_inactive_transition(
                        inactive_state,
                        InactiveState::NoSupportedMetrics,
                        "Accelerator monitoring enabled but no supported metrics were found yet",
                    );
                    return LoopDirective::Continue;
                };

                for device in &snapshot.devices {
                    debug!(
                        "Accelerator monitoring discovered {} ({})",
                        device_label(device),
                        device.name
                    );
                }

                if let Some(directive) =
                    pending_control_directive(context.pause_rx, context.shutdown)
                {
                    return directive;
                }

                if let Err(err) = context
                    .discovery_catalog
                    .upsert_components(state.discovery_entries.clone())
                {
                    error!("Disabling accelerator monitoring because discovery catalog update failed: {err}");
                    return LoopDirective::Stop;
                }

                let discovery_json = match context
                    .discovery_catalog
                    .serialize_current(context.config)
                {
                    Ok(json) => json,
                    Err(err) => {
                        error!("Disabling accelerator monitoring because discovery serialization failed: {err}");
                        return LoopDirective::Stop;
                    }
                };

                match send_outbound_or_control(
                    context.action_tx,
                    OutboundMessage::discovery(context.discovery_topic.to_string(), discovery_json),
                    context.pause_rx,
                    context.shutdown,
                    "Failed to publish accelerator discovery update",
                )
                .await
                {
                    LoopDirective::Continue => {}
                    directive => return directive,
                }

                info!("Accelerator monitoring activated");
                *inactive_state = None;
                let published = publish_snapshot_state(
                    context.action_tx,
                    context.pause_rx,
                    context.shutdown,
                    context.state_topic,
                    &snapshot,
                    &state,
                )
                .await;
                *active_state = Some(state);
                published
            }
            Err(err) => {
                log_inactive_transition(
                    inactive_state,
                    InactiveState::CollectionError(err.clone()),
                    &format!("Accelerator monitoring is still waiting for a usable backend: {err}"),
                );
                LoopDirective::Continue
            }
        },
    }
}

async fn publish_snapshot_state(
    action_tx: &mpsc::Sender<ActionMessage>,
    pause_rx: &mut watch::Receiver<bool>,
    shutdown: &CancellationToken,
    state_topic: &str,
    snapshot: &AcceleratorSnapshot,
    active_state: &ActiveMonitorState,
) -> LoopDirective {
    let payload_map = active_state.payload_from_snapshot(snapshot);
    if payload_map.is_empty() {
        debug!("Skipping accelerator sample because no discovered metrics were present");
        return LoopDirective::Continue;
    }

    let payload = match serde_json::to_string(&payload_map) {
        Ok(payload) => payload,
        Err(err) => {
            error!("Failed to serialize accelerator metrics: {err}");
            return LoopDirective::Continue;
        }
    };

    debug!("Accelerator metrics: {payload}");
    send_outbound_or_control(
        action_tx,
        OutboundMessage::state(state_topic.to_string(), payload),
        pause_rx,
        shutdown,
        "Failed to send accelerator metrics update",
    )
    .await
}

async fn collect_or_control(
    collector: Arc<dyn AcceleratorCollector>,
    pause_rx: &mut watch::Receiver<bool>,
    shutdown: &CancellationToken,
) -> CollectStep {
    if *pause_rx.borrow() {
        return CollectStep::Pause;
    }

    let collect = collector.collect();
    tokio::pin!(collect);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return CollectStep::Shutdown,
            result = &mut collect => return CollectStep::Sample(result),
            changed = pause_rx.changed() => {
                match changed {
                    Ok(()) => {
                        if *pause_rx.borrow() {
                            return CollectStep::Pause;
                        }
                    }
                    Err(_) => return CollectStep::Shutdown,
                }
            }
        }
    }
}

fn pending_control_directive(
    pause_rx: &watch::Receiver<bool>,
    shutdown: &CancellationToken,
) -> Option<LoopDirective> {
    if shutdown.is_cancelled() {
        return Some(LoopDirective::Shutdown);
    }
    if *pause_rx.borrow() {
        return Some(LoopDirective::Pause);
    }
    None
}

async fn send_outbound_or_control(
    action_tx: &mpsc::Sender<ActionMessage>,
    message: OutboundMessage,
    pause_rx: &mut watch::Receiver<bool>,
    shutdown: &CancellationToken,
    failure_message: &str,
) -> LoopDirective {
    if let Some(directive) = pending_control_directive(pause_rx, shutdown) {
        return directive;
    }

    let send = action_tx.send(message);
    tokio::pin!(send);

    loop {
        if let Some(directive) = pending_control_directive(pause_rx, shutdown) {
            return directive;
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return LoopDirective::Shutdown,
            changed = pause_rx.changed() => {
                match changed {
                    Ok(()) => {
                        if *pause_rx.borrow() {
                            return LoopDirective::Pause;
                        }
                    }
                    Err(_) => return LoopDirective::Shutdown,
                }
            }
            result = &mut send => {
                return match result {
                    Ok(()) => LoopDirective::Continue,
                    Err(_) => {
                        error!("{failure_message}: outbound channel closed");
                        LoopDirective::Stop
                    }
                };
            }
        }
    }
}

fn log_inactive_transition(
    inactive_state: &mut Option<InactiveState>,
    next: InactiveState,
    message: &str,
) {
    if inactive_state.as_ref() != Some(&next) {
        warn!("{message}");
    }
    *inactive_state = Some(next);
}

fn metric_descriptors(snapshot: &AcceleratorSnapshot) -> Vec<ProbeDescriptor> {
    let mut descriptors = Vec::new();

    for device in &snapshot.devices {
        for spec in ACCELERATOR_METRICS {
            match metric_reading(device, spec) {
                AcceleratorMetricReading::Value(_) => {
                    descriptors.push(ProbeDescriptor {
                        key: metric_key(device, spec),
                        name: format!("{} {}", device_label(device), spec.display_name),
                        unit: spec.unit,
                        device_class: spec.device_class,
                        startup_source: StartupMetricSource::Value,
                    });
                }
                AcceleratorMetricReading::NotSupported => {}
                AcceleratorMetricReading::ReadError(err) => {
                    let key = metric_key(device, spec);
                    warn!(
                        "Advertising accelerator metric '{key}' despite startup read failure: {err}"
                    );
                    descriptors.push(ProbeDescriptor {
                        key,
                        name: format!("{} {}", device_label(device), spec.display_name),
                        unit: spec.unit,
                        device_class: spec.device_class,
                        startup_source: StartupMetricSource::ReadError,
                    });
                }
            }
        }
    }

    descriptors
}

fn metric_key(device: &AcceleratorDeviceSnapshot, spec: AcceleratorMetricSpec) -> String {
    format!("{}_{}", device_prefix(device), spec.suffix)
}

fn metric_reading(
    device: &AcceleratorDeviceSnapshot,
    spec: AcceleratorMetricSpec,
) -> &AcceleratorMetricReading {
    match spec.kind {
        AcceleratorMetricKind::Utilization => &device.utilization_pct,
        AcceleratorMetricKind::MemoryTotal => &device.memory_total_mb,
        AcceleratorMetricKind::MemoryUsed => &device.memory_used_mb,
        AcceleratorMetricKind::Temperature => &device.temperature_c,
        AcceleratorMetricKind::Power => &device.power_w,
    }
}

fn device_prefix(device: &AcceleratorDeviceSnapshot) -> String {
    match device.kind {
        AcceleratorKind::Gpu => device.stable_key.clone(),
    }
}

fn device_label(device: &AcceleratorDeviceSnapshot) -> String {
    match (device.provider, device.kind) {
        (_, AcceleratorKind::Gpu) => format!("GPU {}", device.index),
    }
}

fn round_to_2dp(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

fn delayed_ticker(interval: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval_at(Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::{
        AcceleratorDeviceSnapshot, AcceleratorKind, AcceleratorMetricReading, AcceleratorProvider,
    };
    use crate::config::AcceleratorMonitorConfig;
    use crate::mqtt::discovery::DiscoveryCatalog;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use tokio::sync::{oneshot, Notify};
    use tokio::time::advance;

    struct FakeCollector {
        samples: Mutex<VecDeque<PendingSample>>,
        shutdown_calls: Mutex<usize>,
    }

    enum PendingSample {
        Ready(Result<AcceleratorSnapshot, String>),
        Wait {
            started_tx: Option<oneshot::Sender<()>>,
            resume: Arc<Notify>,
            result: Result<AcceleratorSnapshot, String>,
        },
    }

    #[async_trait]
    impl AcceleratorCollector for FakeCollector {
        async fn collect(&self) -> Result<AcceleratorSnapshot, String> {
            let sample = self
                .samples
                .lock()
                .expect("samples lock")
                .pop_front()
                .expect("sample should exist");

            match sample {
                PendingSample::Ready(result) => result,
                PendingSample::Wait {
                    started_tx,
                    resume,
                    result,
                } => {
                    if let Some(started_tx) = started_tx {
                        let _ = started_tx.send(());
                    }
                    resume.notified().await;
                    result
                }
            }
        }

        fn shutdown(&self) {
            *self.shutdown_calls.lock().expect("shutdown calls lock") += 1;
        }
    }

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
            accelerator_monitor: AcceleratorMonitorConfig {
                enabled: true,
                provider: crate::config::AcceleratorProviderConfig::Nvidia,
            },
            button: vec![],
            switch: vec![],
            tls: None,
        }
    }

    fn empty_catalog() -> Arc<DiscoveryCatalog> {
        Arc::new(DiscoveryCatalog::new(Vec::<(String, HomeAssistantComponent)>::new()).unwrap())
    }

    fn device(index: u32) -> AcceleratorDeviceSnapshot {
        AcceleratorDeviceSnapshot {
            provider: AcceleratorProvider::Nvidia,
            kind: AcceleratorKind::Gpu,
            index,
            stable_key: format!("gpu_uuid_test_{index}"),
            name: format!("GPU {index}"),
            utilization_pct: AcceleratorMetricReading::Value(73.456),
            memory_total_mb: AcceleratorMetricReading::Value(8192.0),
            memory_used_mb: AcceleratorMetricReading::Value(4096.789),
            temperature_c: AcceleratorMetricReading::Value(61.4),
            power_w: AcceleratorMetricReading::Value(79.995),
        }
    }

    fn device_metric_prefix(index: u32) -> String {
        format!("gpu_uuid_test_{index}")
    }

    fn placeholder_message() -> OutboundMessage {
        OutboundMessage::state("placeholder/topic", "placeholder")
    }

    #[test]
    fn disabled_monitor_does_not_spawn_service() {
        let mut config = test_config();
        config.accelerator_monitor.enabled = false;

        assert!(
            AcceleratorMonitorService::spawn(&config, empty_catalog(), mpsc::channel(1).0)
                .is_none()
        );
    }

    #[test]
    fn activation_from_probe_builds_expected_discovery() {
        let active = ActiveMonitorState::from_probe(
            "testhost",
            "homeassistant/sensor/testhost/accelerator_performance/state",
            &AcceleratorSnapshot {
                devices: vec![device(0)],
            },
        )
        .expect("active state should be created");

        assert_eq!(active.discovery_entries.len(), ACCELERATOR_METRICS.len());
        let prefix = device_metric_prefix(0);
        for (key, discovery) in &active.discovery_entries {
            assert!(key.starts_with(&format!("testhost_{prefix}_")));
            match &discovery.component_type {
                ComponentType::Sensor {
                    state_topic,
                    value_template,
                    ..
                } => {
                    assert_eq!(
                        state_topic,
                        "homeassistant/sensor/testhost/accelerator_performance/state"
                    );
                    assert!(value_template
                        .as_deref()
                        .expect("template")
                        .starts_with(&format!("{{{{ value_json.{prefix}_")));
                }
                _ => panic!("accelerator monitor should only publish sensors"),
            }
        }
    }

    #[test]
    fn payload_serialization_is_flat_and_rounded() {
        let active = ActiveMonitorState::from_probe(
            "testhost",
            "homeassistant/sensor/testhost/accelerator_performance/state",
            &AcceleratorSnapshot {
                devices: vec![device(0)],
            },
        )
        .expect("active state should be created");

        let payload = active.payload_from_snapshot(&AcceleratorSnapshot {
            devices: vec![device(0)],
        });

        let prefix = device_metric_prefix(0);
        assert_eq!(payload.get(&format!("{prefix}_utilization")), Some(&73.46));
        assert_eq!(
            payload.get(&format!("{prefix}_memory_total")),
            Some(&8192.0)
        );
        assert_eq!(
            payload.get(&format!("{prefix}_memory_used")),
            Some(&4096.79)
        );
        assert_eq!(payload.get(&format!("{prefix}_temperature")), Some(&61.4));
        assert_eq!(payload.get(&format!("{prefix}_power")), Some(&80.0));
    }

    #[tokio::test(start_paused = true)]
    async fn startup_failure_retries_then_activates_and_publishes_discovery() {
        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![
                PendingSample::Ready(Err("nvml unavailable".to_string())),
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![device(0)],
                })),
            ])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let catalog = empty_catalog();
        let (tx, mut rx) = mpsc::channel(4);
        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            Arc::clone(&catalog),
            tx,
            collector,
        );
        tokio::task::yield_now().await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        advance(Duration::from_secs(config.update_interval_secs)).await;
        tokio::task::yield_now().await;

        let discovery = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("discovery should arrive")
            .expect("message should exist");
        let state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("state should arrive")
            .expect("message should exist");

        service
            .shutdown()
            .await
            .expect("service should exit cleanly");

        assert!(matches!(discovery, OutboundMessage::Discovery { .. }));
        assert!(matches!(state, OutboundMessage::State { .. }));
        let json = catalog
            .serialize_current(&config)
            .expect("serialize discovery catalog");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse discovery");
        assert!(parsed["cmps"]
            .get(format!("testhost_{}_utilization", device_metric_prefix(0)))
            .is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn later_samples_omit_failing_metrics_but_keep_other_values() {
        let mut probe_device = device(0);
        probe_device.temperature_c =
            AcceleratorMetricReading::ReadError("temperature: timed out".to_string());
        let mut later_device = device(0);
        later_device.temperature_c =
            AcceleratorMetricReading::ReadError("temperature: still failing".to_string());

        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![probe_device],
                })),
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![later_device],
                })),
            ])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(4);
        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            empty_catalog(),
            tx,
            collector,
        );

        let _discovery = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("discovery should arrive")
            .expect("message should exist");
        let _first_state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("state should arrive")
            .expect("message should exist");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "periodic sampling should wait for the configured interval"
        );
        advance(Duration::from_secs(config.update_interval_secs)).await;
        tokio::task::yield_now().await;
        let later_state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second state should arrive")
            .expect("message should exist");

        service
            .shutdown()
            .await
            .expect("service should exit cleanly");

        let payload: serde_json::Value =
            serde_json::from_str(later_state.payload()).expect("payload should parse");
        let prefix = device_metric_prefix(0);
        assert!(payload
            .get(format!("{prefix}_temperature").as_str())
            .is_none());
        assert_eq!(payload[format!("{prefix}_utilization")], 73.46);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_samples_immediately_then_waits_for_next_interval() {
        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![device(0)],
                })),
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![device(0)],
                })),
                PendingSample::Ready(Ok(AcceleratorSnapshot {
                    devices: vec![device(0)],
                })),
            ])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(5);
        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            empty_catalog(),
            tx,
            collector,
        );

        let _discovery = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("discovery should arrive")
            .expect("message should exist");
        let _first_state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first state should arrive")
            .expect("message should exist");
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        service.pause();
        tokio::task::yield_now().await;
        service.resume();

        let resumed_state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("resume state should arrive")
            .expect("message should exist");
        assert!(matches!(resumed_state, OutboundMessage::State { .. }));
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "resume should not trigger an extra immediate tick sample"
        );

        advance(Duration::from_secs(config.update_interval_secs)).await;
        tokio::task::yield_now().await;

        let delayed_state = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("next interval state should arrive")
            .expect("message should exist");

        service
            .shutdown()
            .await
            .expect("service should exit cleanly");

        assert!(matches!(delayed_state, OutboundMessage::State { .. }));
    }

    #[tokio::test]
    async fn shutdown_during_in_flight_collect_does_not_publish_state() {
        let (started_tx, started_rx) = oneshot::channel();
        let resume = Arc::new(Notify::new());
        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![PendingSample::Wait {
                started_tx: Some(started_tx),
                resume: Arc::clone(&resume),
                result: Ok(AcceleratorSnapshot {
                    devices: vec![device(0)],
                }),
            }])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(4);
        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            empty_catalog(),
            tx,
            collector,
        );

        started_rx.await.expect("collect should start");
        let handle = service.shutdown();
        resume.notify_waiters();

        handle.await.expect("service should exit cleanly");
        let maybe_message = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            !matches!(maybe_message, Ok(Some(_))),
            "shutdown should prevent stale accelerator publishes"
        );
    }

    #[tokio::test]
    async fn pause_during_blocked_state_send_drops_stale_state() {
        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![PendingSample::Ready(Ok(
                AcceleratorSnapshot {
                    devices: vec![device(0)],
                },
            ))])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(placeholder_message())
            .await
            .expect("placeholder should queue");

        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            empty_catalog(),
            tx,
            collector,
        );

        tokio::task::yield_now().await;
        let placeholder = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("placeholder should be received")
            .expect("message should exist");
        assert_eq!(placeholder.topic(), "placeholder/topic");

        tokio::task::yield_now().await;
        service.pause();
        tokio::task::yield_now().await;

        let discovery = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("discovery should be received")
            .expect("message should exist");
        assert!(matches!(discovery, OutboundMessage::Discovery { .. }));

        let handle = service.shutdown();
        handle.await.expect("service should exit cleanly");

        let maybe_message = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            !matches!(maybe_message, Ok(Some(OutboundMessage::State { .. }))),
            "pause should cancel the blocked state publish"
        );
    }

    #[tokio::test]
    async fn shutdown_during_blocked_discovery_send_drops_activation_publish() {
        let collector = Arc::new(FakeCollector {
            samples: Mutex::new(VecDeque::from(vec![PendingSample::Ready(Ok(
                AcceleratorSnapshot {
                    devices: vec![device(0)],
                },
            ))])),
            shutdown_calls: Mutex::new(0),
        }) as Arc<dyn AcceleratorCollector>;
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(placeholder_message())
            .await
            .expect("placeholder should queue");

        let service = AcceleratorMonitorService::spawn_with_collector(
            &config,
            empty_catalog(),
            tx,
            collector,
        );

        tokio::task::yield_now().await;
        let handle = service.shutdown();

        let placeholder = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("placeholder should be received")
            .expect("message should exist");
        assert_eq!(placeholder.topic(), "placeholder/topic");

        handle.await.expect("service should exit cleanly");

        let maybe_message = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            !matches!(
                maybe_message,
                Ok(Some(
                    OutboundMessage::Discovery { .. } | OutboundMessage::State { .. }
                ))
            ),
            "shutdown should cancel blocked accelerator activation publishes"
        );
    }
}
