use rumqttc::{AsyncClient, QoS};
use serde::Serialize;
use sysinfo::System;
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info};

#[derive(Serialize)]
struct CpuLoadData {
    load: f32,
}

#[derive(Serialize)]
struct CpuFrequencyData {
    frequency: f32,
}

#[derive(Serialize)]
struct MemoryTotalData {
    total: f32,
}

#[derive(Serialize)]
struct MemoryFreeData {
    free: f32,
}

#[derive(Serialize)]
struct MemoryFreePercentageData {
    free_percentage: f32,
}

pub struct SystemMonitor {
    system: System,
    hostname: String,
    client: AsyncClient,
    last_cpu_update: Instant,
    last_memory_update: Instant,
}

impl SystemMonitor {
    pub fn new(hostname: String, client: AsyncClient) -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        
        Self {
            system,
            hostname,
            client,
            last_cpu_update: Instant::now(),
            last_memory_update: Instant::now(),
        }
    }

    pub async fn run_monitoring_loop(&mut self) {
        let mut cpu_interval = time::interval(Duration::from_secs(60));
        let mut memory_interval = time::interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = cpu_interval.tick() => {
                    if let Err(e) = self.update_cpu_metrics().await {
                        error!("Failed to update CPU metrics: {}", e);
                    }
                }
                _ = memory_interval.tick() => {
                    if let Err(e) = self.update_memory_metrics().await {
                        error!("Failed to update memory metrics: {}", e);
                    }
                }
            }
        }
    }

    async fn update_cpu_metrics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Updating CPU metrics");
        
        // Refresh CPU information
        self.system.refresh_cpu();
        
        // Get load average (1 minute) and normalize by CPU count
        let load_avg = System::load_average();
        let cpu_count = self.system.cpus().len() as f64;
        let cpu_load = (load_avg.one / cpu_count * 100.0) as f32;
        
        // Publish CPU load
        let cpu_load_data = CpuLoadData { load: cpu_load };
        let cpu_load_json = serde_json::to_string(&cpu_load_data)?;
        let cpu_load_topic = format!("homeassistant/sensor/{}/cpu_load/state", self.hostname);
        
        info!("Publishing CPU load: {:.2}% (load avg: {:.2}, cores: {})", cpu_load, load_avg.one, cpu_count);
        self.client.publish(&cpu_load_topic, QoS::AtMostOnce, false, cpu_load_json).await?;
        
        // Get CPU frequency (if available) and convert to GHz
        if let Some(cpu) = self.system.cpus().first() {
            let frequency_mhz = cpu.frequency();
            if frequency_mhz > 0 {
                let frequency_ghz = frequency_mhz as f32 / 1000.0;
                let cpu_freq_data = CpuFrequencyData { frequency: frequency_ghz };
                let cpu_freq_json = serde_json::to_string(&cpu_freq_data)?;
                let cpu_freq_topic = format!("homeassistant/sensor/{}/cpu_frequency/state", self.hostname);
                
                info!("Publishing CPU frequency: {:.2} GHz", frequency_ghz);
                self.client.publish(&cpu_freq_topic, QoS::AtMostOnce, false, cpu_freq_json).await?;
            } else {
                debug!("CPU frequency information not available");
            }
        }
        
        self.last_cpu_update = Instant::now();
        Ok(())
    }

    async fn update_memory_metrics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Updating memory metrics");
        
        // Refresh memory information
        self.system.refresh_memory();
        
        let total_memory_kb = self.system.total_memory();
        let free_memory_kb = self.system.available_memory();
        
        // Convert to GB
        let total_memory_gb = total_memory_kb as f32 / 1_024_000.0;
        let free_memory_gb = free_memory_kb as f32 / 1_024_000.0;
        let free_percentage = (free_memory_kb as f32 / total_memory_kb as f32) * 100.0;
        
        // Publish memory total
        let memory_total_data = MemoryTotalData { total: total_memory_gb };
        let memory_total_json = serde_json::to_string(&memory_total_data)?;
        let memory_total_topic = format!("homeassistant/sensor/{}/memory_total/state", self.hostname);
        
        // Publish memory free
        let memory_free_data = MemoryFreeData { free: free_memory_gb };
        let memory_free_json = serde_json::to_string(&memory_free_data)?;
        let memory_free_topic = format!("homeassistant/sensor/{}/memory_free/state", self.hostname);
        
        // Publish memory free percentage
        let memory_free_pct_data = MemoryFreePercentageData { free_percentage };
        let memory_free_pct_json = serde_json::to_string(&memory_free_pct_data)?;
        let memory_free_pct_topic = format!("homeassistant/sensor/{}/memory_free_pct/state", self.hostname);
        
        info!("Publishing memory metrics - Total: {:.2} GB, Free: {:.2} GB ({:.1}%)", 
              total_memory_gb, free_memory_gb, free_percentage);
        
        self.client.publish(&memory_total_topic, QoS::AtMostOnce, false, memory_total_json).await?;
        self.client.publish(&memory_free_topic, QoS::AtMostOnce, false, memory_free_json).await?;
        self.client.publish(&memory_free_pct_topic, QoS::AtMostOnce, false, memory_free_pct_json).await?;
        
        self.last_memory_update = Instant::now();
        Ok(())
    }
}
