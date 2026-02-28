use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sysinfo::System;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::components::trait_def::{ActionMessage, Component};
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};
use crate::util::helpers::spawn_polling_task;

/// CPU usage sensor.
pub struct CpuSensor {
    name: String,
    unique_id: String,
    state_topic: String,
    update_interval: Duration,
}

impl CpuSensor {
    pub fn new(hostname: &str, update_interval_secs: u64) -> Self {
        Self {
            name: "CPU Usage".to_string(),
            unique_id: format!("{hostname}_cpu_usage"),
            state_topic: format!("homeassistant/sensor/{hostname}/cpu_usage/state"),
            update_interval: Duration::from_secs(update_interval_secs),
        }
    }
}

#[async_trait]
impl Component for CpuSensor {
    fn name(&self) -> &str {
        &self.name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.name.clone(),
            unique_id: self.unique_id.clone(),
            component_type: ComponentType::Sensor {
                state_topic: self.state_topic.clone(),
                device_class: None,
                unit_of_measurement: Some("%".to_string()),
                value_template: None,
                icon: Some("mdi:cpu-64-bit".to_string()),
            },
        }
    }

    fn subscriptions(&self) -> Vec<String> {
        vec![] // Sensors only publish.
    }

    async fn handle_message(
        &self,
        _topic: &str,
        _payload: &str,
        _action_tx: &mpsc::Sender<ActionMessage>,
    ) {
        // Sensors don't receive messages.
    }

    fn spawn_polling(
        self: Arc<Self>,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let interval = self.update_interval;
        let state_topic = self.state_topic.clone();

        // Prime the CPU measurement — the first refresh establishes a baseline
        // so subsequent single calls return meaningful deltas.
        let mut sys = System::new();
        sys.refresh_cpu_usage();

        Some(spawn_polling_task(
            "CPU sensor",
            state_topic,
            interval,
            sys,
            shutdown,
            action_tx,
            |sys| {
                sys.refresh_cpu_usage();
                format!("{:.1}", sys.global_cpu_usage())
            },
        ))
    }

    async fn on_resume(&self, _action_tx: &mpsc::Sender<ActionMessage>) {
        // Polling will restart and publish fresh data.
    }
}

/// Memory usage sensor.
pub struct MemorySensor {
    name: String,
    unique_id: String,
    state_topic: String,
    update_interval: Duration,
}

impl MemorySensor {
    pub fn new(hostname: &str, update_interval_secs: u64) -> Self {
        Self {
            name: "Memory Usage".to_string(),
            unique_id: format!("{hostname}_memory_usage"),
            state_topic: format!("homeassistant/sensor/{hostname}/memory_usage/state"),
            update_interval: Duration::from_secs(update_interval_secs),
        }
    }
}

#[async_trait]
impl Component for MemorySensor {
    fn name(&self) -> &str {
        &self.name
    }

    fn discovery_component(&self) -> HomeAssistantComponent {
        HomeAssistantComponent {
            name: self.name.clone(),
            unique_id: self.unique_id.clone(),
            component_type: ComponentType::Sensor {
                state_topic: self.state_topic.clone(),
                device_class: None,
                unit_of_measurement: Some("%".to_string()),
                value_template: None,
                icon: Some("mdi:memory".to_string()),
            },
        }
    }

    fn subscriptions(&self) -> Vec<String> {
        vec![]
    }

    async fn handle_message(
        &self,
        _topic: &str,
        _payload: &str,
        _action_tx: &mpsc::Sender<ActionMessage>,
    ) {}

    fn spawn_polling(
        self: Arc<Self>,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let interval = self.update_interval;
        let state_topic = self.state_topic.clone();

        Some(spawn_polling_task(
            "Memory sensor",
            state_topic,
            interval,
            System::new(),
            shutdown,
            action_tx,
            |sys| {
                sys.refresh_memory();
                let total = sys.total_memory() as f64;
                let used = sys.used_memory() as f64;
                let usage_pct = if total > 0.0 { (used / total) * 100.0 } else { 0.0 };
                format!("{usage_pct:.1}")
            },
        ))
    }

    async fn on_resume(&self, _action_tx: &mpsc::Sender<ActionMessage>) {}
}
