use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use sysinfo::{CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::components::ambient_light::AmbientLightMonitor;
use crate::components::trait_def::{ActionMessage, Component, OutboundMessage};
use crate::config::AmbientLightMonitorConfig;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};

const BYTES_TO_GB: f32 = 1024.0 * 1024.0 * 1024.0;
const MHZ_TO_GHZ: f32 = 1000.0;
const MIN_DISK_SIZE_BYTES: u64 = 1_073_741_824;
const CPU_REFRESH_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Serialize)]
pub struct SystemPerformanceData {
    pub cpu_load: f32,
    pub cpu_frequency: Option<f32>,
    pub memory_total: f32,
    pub memory_free: f32,
    pub memory_free_percentage: f32,
    pub disk_total: f32,
    pub disk_free: f32,
    pub disk_free_percentage: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient_light: Option<f32>,
}

impl SystemPerformanceData {
    fn from_system_and_disk(system: &System, disk_metrics: DiskMetrics) -> Self {
        let cpu_load = if system.cpus().is_empty() {
            0.0
        } else {
            let total_usage: f32 = system.cpus().iter().map(|cpu| cpu.cpu_usage()).sum();
            total_usage / system.cpus().len() as f32
        };

        let cpu_frequency = system
            .cpus()
            .first()
            .map(|cpu| cpu.frequency())
            .filter(|&frequency| frequency > 0)
            .map(|frequency| round_to_2dp(frequency as f32 / MHZ_TO_GHZ));

        let total_memory = system.total_memory() as f32;
        let free_memory = system.available_memory() as f32;
        let memory_free_percentage = if total_memory > 0.0 {
            (free_memory / total_memory) * 100.0
        } else {
            0.0
        };

        Self {
            cpu_load: round_to_2dp(cpu_load),
            cpu_frequency,
            memory_total: round_to_2dp(total_memory / BYTES_TO_GB),
            memory_free: round_to_2dp(free_memory / BYTES_TO_GB),
            memory_free_percentage: round_to_2dp(memory_free_percentage),
            disk_total: round_to_2dp(disk_metrics.total_gb),
            disk_free: round_to_2dp(disk_metrics.free_gb),
            disk_free_percentage: round_to_2dp(disk_metrics.free_percentage),
            ambient_light: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DiskMetrics {
    total_gb: f32,
    free_gb: f32,
    free_percentage: f32,
}

#[derive(Debug, Clone, Copy)]
struct MetricConfig {
    name: &'static str,
    key: &'static str,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
}

impl MetricConfig {
    const fn new(
        name: &'static str,
        key: &'static str,
        unit: Option<&'static str>,
        device_class: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            key,
            unit,
            device_class,
        }
    }
}

const SYSTEM_METRICS: &[MetricConfig] = &[
    MetricConfig::new("CPU Load", "cpu_load", Some("%"), None),
    MetricConfig::new("CPU Frequency", "cpu_frequency", Some("GHz"), None),
    MetricConfig::new(
        "Memory Total",
        "memory_total",
        Some("GB"),
        Some("data_size"),
    ),
    MetricConfig::new("Memory Free", "memory_free", Some("GB"), Some("data_size")),
    MetricConfig::new("Memory Free %", "memory_free_percentage", Some("%"), None),
    MetricConfig::new("Disk Total", "disk_total", Some("GB"), Some("data_size")),
    MetricConfig::new("Disk Free", "disk_free", Some("GB"), Some("data_size")),
    MetricConfig::new("Disk Free %", "disk_free_percentage", Some("%"), None),
];

struct SystemMonitorState {
    system: System,
    disks: Disks,
    root_disk_index: Option<usize>,
}

impl SystemMonitorState {
    fn new() -> Self {
        let mut system = System::new_with_specifics(system_refresh_kind());
        system.refresh_specifics(system_refresh_kind());
        let disks = Disks::new_with_refreshed_list_specifics(disk_refresh_kind());
        let root_disk_index = find_root_disk_index(&disks);
        debug!("Root disk index: {:?}", root_disk_index);

        Self {
            system,
            disks,
            root_disk_index,
        }
    }

    async fn sample(
        &mut self,
        shutdown: &CancellationToken,
        ambient_light_monitor: Option<&AmbientLightMonitor>,
    ) -> Option<SystemPerformanceData> {
        tokio::select! {
            _ = shutdown.cancelled() => return None,
            _ = tokio::time::sleep(CPU_REFRESH_DELAY) => {}
        }

        self.system.refresh_specifics(system_refresh_kind());
        self.disks.refresh_specifics(false, disk_refresh_kind());

        let disk_metrics = self.disk_metrics();
        let mut performance =
            SystemPerformanceData::from_system_and_disk(&self.system, disk_metrics);
        performance.ambient_light =
            ambient_light_monitor.and_then(|monitor| match monitor.read_value() {
                Ok(value) => Some(value),
                Err(err) => {
                    debug!("Skipping ambient light sample: {err}");
                    None
                }
            });

        Some(performance)
    }

    fn disk_metrics(&self) -> DiskMetrics {
        if let Some(index) = self.root_disk_index {
            if let Some(disk) = self.disks.list().get(index) {
                return calculate_disk_metrics(disk.total_space(), disk.available_space());
            }
        }

        DiskMetrics {
            total_gb: 0.0,
            free_gb: 0.0,
            free_percentage: 0.0,
        }
    }
}

pub struct SystemMonitorComponent {
    hostname: String,
    state_topic: String,
    update_interval: Duration,
    ambient_light_monitor: Option<AmbientLightMonitor>,
}

impl SystemMonitorComponent {
    pub fn new(
        hostname: &str,
        update_interval_secs: u64,
        ambient_light_config: &AmbientLightMonitorConfig,
    ) -> Self {
        Self::with_source(
            hostname,
            Duration::from_secs(update_interval_secs),
            AmbientLightMonitor::from_config(ambient_light_config),
        )
    }

    fn with_source(
        hostname: &str,
        update_interval: Duration,
        ambient_light_monitor: Option<AmbientLightMonitor>,
    ) -> Self {
        Self {
            hostname: hostname.to_string(),
            state_topic: format!("homeassistant/sensor/{hostname}/system_performance/state"),
            update_interval,
            ambient_light_monitor,
        }
    }

    fn discovery_for_metric(&self, metric: MetricConfig) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: metric.name.to_string(),
            unique_id: format!("{}_{}", self.hostname, metric.key),
            component_type: ComponentType::Sensor {
                state_topic: self.state_topic.clone(),
                device_class: metric.device_class.map(str::to_string),
                unit_of_measurement: metric.unit.map(str::to_string),
                value_template: Some(format!("{{{{ value_json.{} }}}}", metric.key)),
                icon: None,
            },
        }
    }
}

#[async_trait]
impl Component for SystemMonitorComponent {
    fn name(&self) -> &str {
        "System Performance"
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        self.discovery_for_metric(SYSTEM_METRICS[0])
    }

    fn discovery_components(&self) -> Vec<(String, HomeAssistantComponent)> {
        let mut components: Vec<_> = SYSTEM_METRICS
            .iter()
            .map(|metric| {
                (
                    format!("{}_{}", self.hostname, metric.key),
                    self.discovery_for_metric(*metric),
                )
            })
            .collect();

        if let Some(ambient_light_monitor) = &self.ambient_light_monitor {
            components
                .push(ambient_light_monitor.discovery_component(&self.hostname, &self.state_topic));
        }

        components
    }

    fn spawn_polling(
        self: Arc<Self>,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let state_topic = self.state_topic.clone();
        let interval = self.update_interval;
        let ambient_light_monitor = self.ambient_light_monitor.clone();

        Some(tokio::spawn(async move {
            let mut state = SystemMonitorState::new();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        debug!("System monitor polling task shutting down");
                        return;
                    }
                    _ = ticker.tick() => {
                        let Some(performance) = state.sample(&shutdown, ambient_light_monitor.as_ref()).await else {
                            debug!("System monitor polling task shutting down");
                            return;
                        };
                        let payload = match serde_json::to_string(&performance) {
                            Ok(payload) => payload,
                            Err(e) => {
                                error!("Failed to serialize system performance metrics: {e}");
                                continue;
                            }
                        };

                        debug!("System performance: {payload}");
                        if let Err(e) = action_tx
                            .send(OutboundMessage::state(state_topic.clone(), payload))
                            .await
                        {
                            error!("Failed to send system performance update: {e}");
                            return;
                        }
                    }
                }
            }
        }))
    }
}

fn system_refresh_kind() -> RefreshKind {
    RefreshKind::nothing()
        .with_memory(MemoryRefreshKind::everything().without_swap())
        .with_cpu(CpuRefreshKind::everything())
}

fn disk_refresh_kind() -> DiskRefreshKind {
    DiskRefreshKind::nothing().with_storage()
}

fn find_root_disk_index(disks: &Disks) -> Option<usize> {
    let disk_list = disks.list();

    disk_list
        .iter()
        .enumerate()
        .find(|(_, disk)| {
            let mount_point = disk.mount_point().to_str().unwrap_or("");
            (mount_point == "/sysroot" || mount_point == "/")
                && disk.total_space() >= MIN_DISK_SIZE_BYTES
        })
        .map(|(idx, _)| idx)
        .or_else(|| {
            disk_list
                .iter()
                .enumerate()
                .filter(|(_, disk)| disk.total_space() >= MIN_DISK_SIZE_BYTES)
                .max_by_key(|(_, disk)| disk.total_space())
                .map(|(idx, _)| idx)
        })
}

fn calculate_disk_metrics(total_bytes: u64, available_bytes: u64) -> DiskMetrics {
    let total_gb = total_bytes as f32 / BYTES_TO_GB;
    let free_gb = available_bytes as f32 / BYTES_TO_GB;
    let free_percentage = if total_gb > 0.0 {
        (free_gb / total_gb) * 100.0
    } else {
        0.0
    };

    DiskMetrics {
        total_gb,
        free_gb,
        free_percentage,
    }
}

fn round_to_2dp(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ambient_light::AMBIENT_LIGHT_METRIC_KEY;
    use tempfile::tempdir;

    #[test]
    fn system_performance_serializes_old_field_names() {
        let payload = SystemPerformanceData {
            cpu_load: 12.34,
            cpu_frequency: Some(2.4),
            memory_total: 32.0,
            memory_free: 12.0,
            memory_free_percentage: 37.5,
            disk_total: 512.0,
            disk_free: 128.0,
            disk_free_percentage: 25.0,
            ambient_light: Some(321.0),
        };

        let value = serde_json::to_value(payload).expect("serialize metrics");
        assert!(value.get("cpu_load").is_some());
        assert!(value.get("cpu_frequency").is_some());
        assert!(value.get("memory_total").is_some());
        assert!(value.get("memory_free").is_some());
        assert!(value.get("memory_free_percentage").is_some());
        assert!(value.get("disk_total").is_some());
        assert!(value.get("disk_free").is_some());
        assert!(value.get("disk_free_percentage").is_some());
        assert_eq!(
            value
                .get(AMBIENT_LIGHT_METRIC_KEY)
                .and_then(|value| value.as_f64()),
            Some(321.0)
        );
    }

    #[test]
    fn system_monitor_discovery_uses_single_state_topic_with_templates() {
        let monitor =
            SystemMonitorComponent::new("testhost", 60, &AmbientLightMonitorConfig::default());
        let entries = monitor.discovery_components();

        assert_eq!(entries.len(), 8);
        for (key, component) in &entries {
            assert!(key.starts_with("testhost_"));
            match &component.component_type {
                ComponentType::Sensor {
                    state_topic,
                    value_template,
                    ..
                } => {
                    assert_eq!(
                        state_topic,
                        "homeassistant/sensor/testhost/system_performance/state"
                    );
                    assert!(value_template
                        .as_ref()
                        .expect("template should exist")
                        .starts_with("{{ value_json."));
                }
                _ => panic!("system monitor should only produce sensors"),
            }
        }

        let memory_total = entries
            .iter()
            .find(|(key, _)| key == "testhost_memory_total")
            .expect("memory_total discovery");
        match &memory_total.1.component_type {
            ComponentType::Sensor {
                device_class,
                unit_of_measurement,
                value_template,
                ..
            } => {
                assert_eq!(device_class.as_deref(), Some("data_size"));
                assert_eq!(unit_of_measurement.as_deref(), Some("GB"));
                assert_eq!(
                    value_template.as_deref(),
                    Some("{{ value_json.memory_total }}")
                );
            }
            _ => panic!("memory_total should be a sensor"),
        }
    }

    #[tokio::test]
    async fn system_monitor_publishes_first_update_without_waiting_for_interval() {
        let monitor = Arc::new(SystemMonitorComponent::new(
            "testhost",
            60,
            &AmbientLightMonitorConfig::default(),
        ));
        let (tx, mut rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let handle = Arc::clone(&monitor)
            .spawn_polling(tx, shutdown.clone())
            .expect("system monitor should spawn polling task");

        let message = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first system monitor update should arrive promptly")
            .expect("system monitor should publish a message");

        shutdown.cancel();
        let _ = handle.await;

        assert_eq!(
            message.topic(),
            "homeassistant/sensor/testhost/system_performance/state"
        );

        let payload: serde_json::Value =
            serde_json::from_str(message.payload()).expect("payload should be valid JSON");
        for field in [
            "cpu_load",
            "cpu_frequency",
            "memory_total",
            "memory_free",
            "memory_free_percentage",
            "disk_total",
            "disk_free",
            "disk_free_percentage",
        ] {
            assert!(payload.get(field).is_some(), "missing field {field}");
        }
        assert!(payload.get(AMBIENT_LIGHT_METRIC_KEY).is_none());
    }

    #[tokio::test]
    async fn system_monitor_omits_ambient_light_after_read_failure_but_keeps_publishing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("iio:device0").join("in_illuminance_input");
        std::fs::create_dir_all(path.parent().expect("ambient light parent")).unwrap();
        std::fs::write(&path, "111.11\n").unwrap();

        let source = AmbientLightMonitor::explicit(path.clone()).expect("ambient light source");
        let monitor = Arc::new(SystemMonitorComponent::with_source(
            "testhost",
            Duration::from_millis(50),
            Some(source),
        ));
        let (tx, mut rx) = mpsc::channel(4);
        let shutdown = CancellationToken::new();
        let handle = Arc::clone(&monitor)
            .spawn_polling(tx, shutdown.clone())
            .expect("system monitor should spawn polling task");

        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("first update should arrive promptly")
            .expect("first message");
        let first_payload: serde_json::Value =
            serde_json::from_str(first.payload()).expect("payload should be valid JSON");
        assert_eq!(
            first_payload
                .get(AMBIENT_LIGHT_METRIC_KEY)
                .and_then(|value| value.as_f64()),
            Some(111.11)
        );

        std::fs::remove_file(&path).expect("remove ambient light file");

        let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("second update should still arrive")
            .expect("second message");

        shutdown.cancel();
        let _ = handle.await;

        let second_payload: serde_json::Value =
            serde_json::from_str(second.payload()).expect("payload should be valid JSON");
        assert!(second_payload.get("cpu_load").is_some());
        assert!(second_payload.get(AMBIENT_LIGHT_METRIC_KEY).is_none());
    }
}
