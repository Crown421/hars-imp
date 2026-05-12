use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zbus::zvariant::OwnedFd;
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
    /// Held inhibitor FD. Dropping releases the lock.
    inhibitor_fd: Mutex<Option<OwnedFd>>,
}

impl PowerMonitor {
    /// Create a new power monitor.
    ///
    /// Returns the monitor and a broadcast receiver for power events.
    pub async fn new(
        system_conn: Connection,
    ) -> Result<(Self, broadcast::Receiver<PowerEvent>), crate::error::DbusError> {
        let (power_tx, power_rx) = broadcast::channel(8);

        Ok((Self::with_sender(system_conn, power_tx), power_rx))
    }

    fn with_sender(system_conn: Connection, power_tx: broadcast::Sender<PowerEvent>) -> Self {
        Self {
            system_conn,
            power_tx,
            inhibitor_fd: Mutex::new(None),
        }
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

                // Release the inhibitor so the system can actually suspend.
                self.release_inhibitor();
            } else {
                info!("System resumed from suspend");
                let _ = self.power_tx.send(PowerEvent::Resuming);

                // Re-acquire inhibitor after resume.
                if let Err(e) = self.acquire_inhibitor().await {
                    warn!("Failed to re-acquire sleep inhibitor after resume: {e}");
                }
            }
        }

        warn!("Power monitor signal stream ended");
        Ok(())
    }

    /// Acquire a delay-mode sleep inhibitor from logind.
    ///
    /// The returned FD is stored in `self.inhibitor_fd`. The lock is held
    /// until `release_inhibitor()` is called (or the monitor is dropped).
    async fn acquire_inhibitor(&self) -> Result<(), crate::error::DbusError> {
        let proxy: zbus::Proxy = zbus::proxy::Builder::new(&self.system_conn)
            .destination("org.freedesktop.login1")?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .build()
            .await?;

        let fd: OwnedFd = proxy
            .call(
                "Inhibit",
                &("sleep", "hars-imp", "Publish status before sleep", "delay"),
            )
            .await?;

        // Store the FD so it stays alive until we explicitly release it.
        if let Ok(mut guard) = self.inhibitor_fd.lock() {
            *guard = Some(fd);
        }

        info!("Acquired sleep inhibitor (delay mode)");
        Ok(())
    }

    /// Release the sleep inhibitor by dropping the stored FD.
    fn release_inhibitor(&self) {
        if let Ok(mut guard) = self.inhibitor_fd.lock() {
            if guard.take().is_some() {
                info!("Released sleep inhibitor");
            }
        }
    }
}

/// Handle for the supervised power monitor task.
pub struct PowerMonitorSupervisor {
    pub receiver: broadcast::Receiver<PowerEvent>,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl PowerMonitorSupervisor {
    /// Spawn a supervised power monitor loop.
    pub fn spawn() -> Self {
        let (power_tx, receiver) = broadcast::channel(8);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(supervise_power_monitor(
            power_tx,
            shutdown.clone(),
            Duration::from_secs(1),
            Duration::from_secs(60),
            run_power_monitor_once,
        ));

        Self {
            receiver,
            shutdown,
            handle,
        }
    }

    /// Cancel the supervisor and wait briefly for it to exit.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        match tokio::time::timeout(Duration::from_secs(2), self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Power monitor supervisor task failed during shutdown: {e}"),
            Err(_) => warn!("Timed out waiting for power monitor supervisor shutdown"),
        }
    }
}

async fn run_power_monitor_once(
    power_tx: broadcast::Sender<PowerEvent>,
) -> Result<(), crate::error::DbusError> {
    let conn = crate::dbus::client::system_connection().await?;
    info!("D-Bus system connection established for power monitoring");
    let monitor = PowerMonitor::with_sender(conn, power_tx);
    monitor.run().await
}

async fn supervise_power_monitor<F, Fut>(
    power_tx: broadcast::Sender<PowerEvent>,
    shutdown: CancellationToken,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut run_once: F,
) where
    F: FnMut(broadcast::Sender<PowerEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<(), crate::error::DbusError>>,
{
    let mut attempt: u64 = 1;
    let mut backoff = initial_backoff;

    loop {
        let result = tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Power monitor supervisor shutting down");
                return;
            }
            result = run_once(power_tx.clone()) => result,
        };

        match result {
            Ok(()) => warn!(
                "Power monitor exited unexpectedly; restarting attempt {attempt} in {backoff:?}"
            ),
            Err(e) => {
                warn!("Power monitor failed: {e}; restarting attempt {attempt} in {backoff:?}")
            }
        }

        let delay = backoff;
        attempt += 1;
        backoff = (backoff * 2).min(max_backoff);

        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Power monitor supervisor shutting down");
                return;
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn supervisor_restarts_after_monitor_exit() {
        let (tx, _rx) = broadcast::channel(8);
        let shutdown = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_task = Arc::clone(&attempts);

        let handle = tokio::spawn(supervise_power_monitor(
            tx,
            shutdown.clone(),
            Duration::from_millis(10),
            Duration::from_millis(20),
            move |_| {
                let attempts = Arc::clone(&attempts_for_task);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("supervisor should restart after an unexpected exit");

        shutdown.cancel();
        handle.await.expect("supervisor task should exit cleanly");
    }

    #[tokio::test]
    async fn supervisor_stops_without_restart_after_shutdown() {
        let (tx, _rx) = broadcast::channel(8);
        let shutdown = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_task = Arc::clone(&attempts);

        let handle = tokio::spawn(supervise_power_monitor(
            tx,
            shutdown.clone(),
            Duration::from_secs(10),
            Duration::from_secs(10),
            move |_| {
                let attempts = Arc::clone(&attempts_for_task);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            },
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first monitor attempt should run");

        shutdown.cancel();
        handle.await.expect("supervisor task should exit cleanly");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
