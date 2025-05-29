use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use sysinfo::{Disks, System};
use tokio::time::{self, Duration};
use tracing::{debug, error, info};
use crate::ha_mqtt::{HomeAssistantComponent};
use crate::utils::Config;

#[derive(Serialize)]
pub struct SystemPerformanceData {
    pub cpu_load: f32,
    pub cpu_frequency: Option<f32>,
    pub memory_total: f32,
    pub memory_free: f32,
    pub memory_free_percentage: f32,
    pub disk_total: f32,
    pub disk_free: f32,
    pub disk_free_percentage: f32,
}

impl SystemPerformanceData {
    // Add this new method
    pub fn from_system(system: &System) -> Self {
        // Get CPU metrics - calculate average CPU usage across all cores
        let cpu_load = if !system.cpus().is_empty() {
            let total_usage: f32 = system.cpus().iter()
                .map(|cpu| cpu.cpu_usage())
                .sum();
            total_usage / system.cpus().len() as f32
        } else {
            0.0
        };
        
        // Get CPU frequency (if available) and convert to GHz
        let cpu_frequency = system.cpus().first()
            .map(|cpu| cpu.frequency())
            .filter(|&freq| freq > 0)
            .map(|freq| freq as f32 / 1000.0);
        
        // Get memory metrics
        let total_memory = system.total_memory();
        let free_memory = system.available_memory();
        
        // Convert to GB
        let total_memory_gb = total_memory as f32 / 1024.0 / 1024.0 / 1024.0;
        let free_memory_gb = free_memory as f32 / 1024.0 / 1024.0 / 1024.0;
        let free_percentage = (free_memory as f32 / total_memory as f32) * 100.0;
        
        // Get disk metrics for root filesystem
        // Try to find /sysroot first (for immutable OS like Fedora Silverblue/Kinoite)
        // Fall back to / if /sysroot is not available
        let disks = Disks::new_with_refreshed_list();
        let (disk_total_gb, disk_free_gb, disk_free_percentage) = disks
            .iter()
            .find(|disk| {
                let mount_point = disk.mount_point().to_str().unwrap_or("");
                mount_point == "/sysroot" || mount_point == "/"
            })
            .and_then(|disk| {
                // Skip very small filesystems (less than 1GB) like composefs overlays
                if disk.total_space() < 1_073_741_824 { // 1GB in bytes
                    None
                } else {
                    let total = disk.total_space() as f32 / 1024.0 / 1024.0 / 1024.0;
                    let available = disk.available_space() as f32 / 1024.0 / 1024.0 / 1024.0;
                    let percentage = if total > 0.0 { (available / total) * 100.0 } else { 0.0 };
                    Some((total, available, percentage))
                }
            })
            .unwrap_or_else(|| {
                // If no suitable disk found, try to find the largest disk as fallback
                disks
                    .iter()
                    .filter(|disk| disk.total_space() >= 1_073_741_824) // At least 1GB
                    .max_by_key(|disk| disk.total_space())
                    .map(|disk| {
                        let total = disk.total_space() as f32 / 1024.0 / 1024.0 / 1024.0;
                        let available = disk.available_space() as f32 / 1024.0 / 1024.0 / 1024.0;
                        let percentage = if total > 0.0 { (available / total) * 100.0 } else { 0.0 };
                        (total, available, percentage)
                    })
                    .unwrap_or((0.0, 0.0, 0.0))
            });
        
        // Return performance data with values rounded to 2 decimal places
        Self {
            cpu_load: (cpu_load * 100.0).round() / 100.0,
            cpu_frequency: cpu_frequency.map(|f| (f * 100.0).round() / 100.0),
            memory_total: (total_memory_gb * 100.0).round() / 100.0,
            memory_free: (free_memory_gb * 100.0).round() / 100.0,
            memory_free_percentage: (free_percentage * 100.0).round() / 100.0,
            disk_total: (disk_total_gb * 100.0).round() / 100.0,
            disk_free: (disk_free_gb * 100.0).round() / 100.0,
            disk_free_percentage: (disk_free_percentage * 100.0).round() / 100.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricConfig {
    pub name: &'static str,
    pub json_field: &'static str,
    pub unit: Option<&'static str>,
    pub device_class: Option<&'static str>,
}

impl MetricConfig {
    pub const fn new(
        name: &'static str,
        json_field: &'static str,
        unit: Option<&'static str>,
        device_class: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            json_field,
            unit,
            device_class,
        }
    }
}

pub const SYSTEM_METRICS: &[MetricConfig] = &[
    MetricConfig::new("CPU Load", "cpu_load", Some("%"), None),
    MetricConfig::new("CPU Frequency", "cpu_frequency", Some("GHz"), None),
    MetricConfig::new("Memory Total", "memory_total", Some("GB"), Some("data_size")),
    MetricConfig::new("Memory Free", "memory_free", Some("GB"), Some("data_size")),
    MetricConfig::new("Memory Free %", "memory_free_percentage", Some("%"), None),
    MetricConfig::new("Disk Total", "disk_total", Some("GB"), Some("data_size")),
    MetricConfig::new("Disk Free", "disk_free", Some("GB"), Some("data_size")),
    MetricConfig::new("Disk Free %", "disk_free_percentage", Some("%"), None),
];

pub struct SystemMonitor {
    system: System,
    sensor_topic: String,
    client: AsyncClient,
}

impl SystemMonitor {
    pub fn new(sensor_topic_base: String, client: AsyncClient) -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        let sensor_topic = format!("{}/system_performance/state", sensor_topic_base);
        
        Self {
            system,
            sensor_topic,
            client,
        }
    }

    pub async fn run_monitoring_loop(&mut self) {
        // For accurate CPU usage, we need to refresh again after a small delay
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        self.system.refresh_cpu();
        
        let mut interval = time::interval(Duration::from_secs(60));

        loop {
            interval.tick().await;
            if let Err(e) = self.update_system_metrics().await {
                error!("Failed to update system metrics: {}", e);
            }
        }
    }

    async fn update_system_metrics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Updating system metrics");
        
        // Refresh CPU and memory information
        self.system.refresh_cpu();
        self.system.refresh_memory();
        
        // Create performance data using the new method
        let performance_data = SystemPerformanceData::from_system(&self.system);
        
        info!("Publishing system performance - CPU: {:.2}%, Freq: {:?} GHz, Memory: {:.2}/{:.2} GB ({:.1}% free), Disk: {:.2}/{:.2} GB ({:.1}% free)", 
            performance_data.cpu_load, 
            performance_data.cpu_frequency, 
            performance_data.memory_free, 
            performance_data.memory_total, 
            performance_data.memory_free_percentage,
            performance_data.disk_free,
            performance_data.disk_total,
            performance_data.disk_free_percentage);

        // Publish to single topic
        let performance_json = serde_json::to_string(&performance_data)?;
        
        self.client.publish(&self.sensor_topic, QoS::AtMostOnce, false, performance_json).await?;
        
        Ok(())
    }
}

/// Creates system monitoring sensor components
pub fn create_system_sensor_components(config: &Config) -> Vec<(String, HomeAssistantComponent)> {
    let mut components = Vec::new();
    let state_topic = format!("{}/system_performance/state", config.sensor_topic_base);
    
    for metric in SYSTEM_METRICS {
        let component_id = metric.json_field.to_string();
        let component = HomeAssistantComponent::sensor(
            metric.name.to_string(),
            component_id.clone(),
            state_topic.clone(),
            metric.device_class.map(|s| s.to_string()),
            metric.unit.map(|s| s.to_string()),
            format!("{{{{ value_json.{} }}}}", metric.json_field),
        );
        components.push((component_id, component));
    }
    
    components
}
