use std::sync::Arc;
use std::thread;

use async_trait::async_trait;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::Nvml;
use tokio::sync::oneshot;
use tracing::warn;

use crate::config::{AcceleratorMonitorConfig, AcceleratorProviderConfig};
use crate::util::helpers::slugify;

const BYTES_TO_MB: f32 = 1024.0 * 1024.0;
const MILLIWATTS_TO_WATTS: f32 = 1000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceleratorProvider {
    Nvidia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceleratorKind {
    Gpu,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceleratorSnapshot {
    pub devices: Vec<AcceleratorDeviceSnapshot>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceleratorDeviceSnapshot {
    pub provider: AcceleratorProvider,
    pub kind: AcceleratorKind,
    pub index: u32,
    pub stable_key: String,
    pub name: String,
    pub utilization_pct: AcceleratorMetricReading,
    pub memory_total_mb: AcceleratorMetricReading,
    pub memory_used_mb: AcceleratorMetricReading,
    pub temperature_c: AcceleratorMetricReading,
    pub power_w: AcceleratorMetricReading,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AcceleratorMetricReading {
    Value(f32),
    NotSupported,
    ReadError(String),
}

pub(crate) trait AcceleratorBackend: Send + Sync {
    fn collect(&self) -> Result<AcceleratorSnapshot, String>;
}

#[async_trait]
pub(crate) trait AcceleratorCollector: Send + Sync {
    async fn collect(&self) -> Result<AcceleratorSnapshot, String>;
    fn shutdown(&self);
}

pub(crate) fn initialize_backend(
    config: &AcceleratorMonitorConfig,
) -> Result<Option<Box<dyn AcceleratorBackend>>, String> {
    if !config.enabled {
        return Ok(None);
    }

    match config.provider {
        AcceleratorProviderConfig::Nvidia => {
            NvidiaAcceleratorBackend::new().map(|backend| Some(Box::new(backend) as Box<_>))
        }
    }
}

pub(crate) fn start_collector(config: AcceleratorMonitorConfig) -> Arc<dyn AcceleratorCollector> {
    Arc::new(ThreadedAcceleratorCollector::new(config))
}

struct NvidiaAcceleratorBackend {
    nvml: Nvml,
}

impl NvidiaAcceleratorBackend {
    fn new() -> Result<Self, String> {
        let nvml = Nvml::init().map_err(|err| format!("failed to initialize NVML: {err}"))?;
        Ok(Self { nvml })
    }
}

impl AcceleratorBackend for NvidiaAcceleratorBackend {
    fn collect(&self) -> Result<AcceleratorSnapshot, String> {
        let device_count = self
            .nvml
            .device_count()
            .map_err(|err| format!("failed to enumerate NVIDIA devices: {err}"))?;
        let mut devices = Vec::new();

        for index in 0..device_count {
            let device = match self.nvml.device_by_index(index) {
                Ok(device) => device,
                Err(err) => {
                    warn!("Skipping NVIDIA device at index {index}: {err}");
                    continue;
                }
            };

            let memory_info = device.memory_info();
            let (memory_total_mb, memory_used_mb) = match memory_info {
                Ok(memory) => (
                    AcceleratorMetricReading::Value(memory.total as f32 / BYTES_TO_MB),
                    AcceleratorMetricReading::Value(memory.used as f32 / BYTES_TO_MB),
                ),
                Err(err) => {
                    let total = classify_metric_error("memory total", &err);
                    let used = classify_metric_error("memory used", &err);
                    (total, used)
                }
            };

            devices.push(AcceleratorDeviceSnapshot {
                provider: AcceleratorProvider::Nvidia,
                kind: AcceleratorKind::Gpu,
                index,
                stable_key: stable_device_key(&device, index),
                name: device.name().unwrap_or_else(|_| format!("GPU {index}")),
                utilization_pct: classify_metric_result(
                    "utilization",
                    device.utilization_rates().map(|rates| rates.gpu as f32),
                ),
                memory_total_mb,
                memory_used_mb,
                temperature_c: classify_metric_result(
                    "temperature",
                    device
                        .temperature(TemperatureSensor::Gpu)
                        .map(|value| value as f32),
                ),
                power_w: classify_metric_result(
                    "power",
                    device
                        .power_usage()
                        .map(|value| value as f32 / MILLIWATTS_TO_WATTS),
                ),
            });
        }

        Ok(AcceleratorSnapshot { devices })
    }
}

fn stable_device_key(device: &nvml_wrapper::Device<'_>, index: u32) -> String {
    if let Ok(uuid) = device.uuid() {
        return format!("gpu_{}", slugify(&uuid));
    }

    if let Ok(pci_info) = device.pci_info() {
        return format!("gpu_{}", slugify(&pci_info.bus_id));
    }

    format!("gpu_index_{index}")
}

fn classify_metric_result(
    metric_name: &str,
    result: Result<f32, NvmlError>,
) -> AcceleratorMetricReading {
    match result {
        Ok(value) => AcceleratorMetricReading::Value(value),
        Err(err) => classify_metric_error(metric_name, &err),
    }
}

fn classify_metric_error(metric_name: &str, err: &NvmlError) -> AcceleratorMetricReading {
    match err {
        NvmlError::NotSupported => AcceleratorMetricReading::NotSupported,
        _ => AcceleratorMetricReading::ReadError(format!("{metric_name}: {err}")),
    }
}

struct ThreadedAcceleratorCollector {
    command_tx: std::sync::mpsc::Sender<CollectorCommand>,
}

enum CollectorCommand {
    Collect {
        response_tx: oneshot::Sender<Result<AcceleratorSnapshot, String>>,
    },
    Shutdown,
}

impl ThreadedAcceleratorCollector {
    fn new(config: AcceleratorMonitorConfig) -> Self {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        thread::spawn(move || collector_thread(config, command_rx));
        Self { command_tx }
    }
}

#[async_trait]
impl AcceleratorCollector for ThreadedAcceleratorCollector {
    async fn collect(&self) -> Result<AcceleratorSnapshot, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(CollectorCommand::Collect { response_tx })
            .map_err(|_| "accelerator collector worker is not running".to_string())?;

        response_rx
            .await
            .map_err(|_| "accelerator collector worker dropped the response".to_string())?
    }

    fn shutdown(&self) {
        let _ = self.command_tx.send(CollectorCommand::Shutdown);
    }
}

impl Drop for ThreadedAcceleratorCollector {
    fn drop(&mut self) {
        let _ = self.command_tx.send(CollectorCommand::Shutdown);
    }
}

fn collector_thread(
    config: AcceleratorMonitorConfig,
    command_rx: std::sync::mpsc::Receiver<CollectorCommand>,
) {
    let mut backend: Option<Box<dyn AcceleratorBackend>> = None;

    while let Ok(command) = command_rx.recv() {
        match command {
            CollectorCommand::Collect { response_tx } => {
                let result = collect_from_backend(&config, &mut backend);
                let _ = response_tx.send(result);
            }
            CollectorCommand::Shutdown => return,
        }
    }
}

fn collect_from_backend(
    config: &AcceleratorMonitorConfig,
    backend: &mut Option<Box<dyn AcceleratorBackend>>,
) -> Result<AcceleratorSnapshot, String> {
    if backend.is_none() {
        *backend = initialize_backend(config)?;
    }

    let backend = backend
        .as_ref()
        .ok_or_else(|| "accelerator monitoring is disabled".to_string())?;
    backend.collect()
}
