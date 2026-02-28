use std::time::Duration;

use futures::StreamExt;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use zbus::Connection;

/// Events emitted by the power monitor.
#[derive(Debug, Clone)]
pub enum PowerEvent {
    /// The system is about to suspend.
    Suspending,

    /// The system has resumed from suspend.
    Resuming,
}

/// Monitors systemd-logind for suspend/resume signals.
///
/// Acquires a sleep inhibitor lock (delay mode) so we get time to
/// clean up before the system actually suspends.
pub struct PowerMonitor {
    system_conn: Connection,
    power_tx: broadcast::Sender<PowerEvent>,
}

impl PowerMonitor {
    /// Create a new power monitor.
    ///
    /// Returns the monitor and a broadcast receiver for power events.
    pub async fn new(
        system_conn: Connection,
    ) -> Result<(Self, broadcast::Receiver<PowerEvent>), crate::error::DbusError> {
        let (power_tx, power_rx) = broadcast::channel(8);

        Ok((
            Self {
                system_conn,
                power_tx,
            },
            power_rx,
        ))
    }

    /// Get an additional receiver for power events.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> broadcast::Receiver<PowerEvent> {
        self.power_tx.subscribe()
    }

    /// Run the power monitoring loop.
    ///
    /// This listens for `PrepareForSleep` signals from systemd-logind
    /// and broadcasts `PowerEvent`s.
    pub async fn run(self) -> Result<(), crate::error::DbusError> {
        info!("Starting power monitor");

        // Acquire sleep inhibitor (delay mode) so we get a chance to clean up.
        if let Err(e) = self.acquire_inhibitor().await {
            warn!("Failed to acquire sleep inhibitor: {e}");
        }

        // Listen for PrepareForSleep signals.
        let proxy: zbus::Proxy = zbus::proxy::Builder::new(&self.system_conn)
            .destination("org.freedesktop.login1")?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .build()
            .await?;

        let mut stream = proxy.receive_signal("PrepareForSleep").await?;

        while let Some(signal) = stream.next().await {
            let body: zbus::message::Body = signal.body();
            let suspending: bool = match body.deserialize() {
                Ok(val) => val,
                Err(e) => {
                    error!("Failed to deserialize PrepareForSleep signal: {e}");
                    continue;
                }
            };

            if suspending {
                info!("System preparing to suspend");
                let _ = self.power_tx.send(PowerEvent::Suspending);
            } else {
                info!("System resumed from suspend");
                let _ = self.power_tx.send(PowerEvent::Resuming);

                // Re-acquire inhibitor after resume.
                if let Err(e) = self.acquire_inhibitor().await {
                    warn!("Failed to re-acquire sleep inhibitor after resume: {e}");
                }
            }
        }

        debug!("Power monitor signal stream ended");
        Ok(())
    }

    /// Acquire a delay-mode sleep inhibitor from logind.
    async fn acquire_inhibitor(&self) -> Result<(), crate::error::DbusError> {
        let proxy: zbus::Proxy = zbus::proxy::Builder::new(&self.system_conn)
            .destination("org.freedesktop.login1")?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .build()
            .await?;

        let _reply: zbus::zvariant::OwnedFd = proxy
            .call("Inhibit", &("sleep", "hars-imp", "Publish status before sleep", "delay"))
            .await?;

        info!("Acquired sleep inhibitor (delay mode)");
        Ok(())
    }
}

/// Attempt to reconnect to the system D-Bus with retries.
#[allow(dead_code)]
pub async fn reconnect_system_dbus(max_retries: u32) -> Result<Connection, crate::error::DbusError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match Connection::system().await {
            Ok(conn) => {
                info!("D-Bus system connection established (attempt {attempt})");
                return Ok(conn);
            }
            Err(e) => {
                if attempt >= max_retries {
                    error!("Failed to connect to D-Bus after {max_retries} attempts");
                    return Err(crate::error::DbusError::ConnectionFailed);
                }
                warn!("D-Bus connection attempt {attempt} failed: {e}");
                let delay = Duration::from_secs(1 << attempt.min(5));
                tokio::time::sleep(delay).await;
            }
        }
    }
}
