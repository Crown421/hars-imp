use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use sysinfo::System;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

#[derive(Serialize)]
pub struct SystemPerformanceData {
    pub cpu_load: f32,
    pub cpu_frequency: Option<f32>,
    pub memory_total: f32,
    pub memory_free: f32,
    pub memory_free_percentage: f32,
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
];

pub struct SystemMonitor {
    system: System,
    hostname: String,
    client: AsyncClient,
}

impl SystemMonitor {
    pub fn new(hostname: String, client: AsyncClient) -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        
        Self {
            system,
            hostname,
            client,
        }
    }

    pub async fn run_monitoring_loop(&mut self) {
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
        
        // Refresh all system information
        self.system.refresh_all();
        
        // Get CPU metrics
        let load_avg = System::load_average();
        let cpu_count = self.system.cpus().len() as f64;
        let cpu_load = (load_avg.one / cpu_count * 100.0) as f32;
        
        // Get CPU frequency (if available) and convert to GHz
        let cpu_frequency = self.system.cpus().first()
            .map(|cpu| cpu.frequency())
            .filter(|&freq| freq > 0)
            .map(|freq| freq as f32 / 1000.0);
        
        // Get memory metrics
        let total_memory = self.system.total_memory();
        let free_memory = self.system.available_memory();
        
        // Convert to GB
        let total_memory_gb = total_memory as f32 / 1024.0 / 1024.0 / 1024.0;
        let free_memory_gb = free_memory as f32 / 1024.0 / 1024.0 / 1024.0;
        let free_percentage = (free_memory as f32 / total_memory as f32) * 100.0;
        
        // Create combined performance data
        let performance_data = SystemPerformanceData {
            cpu_load,
            cpu_frequency,
            memory_total: total_memory_gb,
            memory_free: free_memory_gb,
            memory_free_percentage: free_percentage,
        };
        
        // Publish to single topic
        let performance_json = serde_json::to_string(&performance_data)?;
        let performance_topic = format!("homeassistant/sensor/{}/system_performance/state", self.hostname);
        
        info!("Publishing system performance - CPU: {:.2}%, Freq: {:?} GHz, Memory: {:.2}/{:.2} GB ({:.1}% free)", 
              cpu_load, cpu_frequency, free_memory_gb, total_memory_gb, free_percentage);
        
        self.client.publish(&performance_topic, QoS::AtMostOnce, false, performance_json).await?;
        
        Ok(())
    }
}
