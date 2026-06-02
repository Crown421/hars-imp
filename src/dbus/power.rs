use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zbus::zvariant::OwnedFd;
use zbus::Connection;

/// Events emitted by the power monitor.
#[derive(Debug, Clone)]
pub enum PowerEvent {
    /// The system is about to suspend.
    Suspending(LogindDelayHold),

    /// The system has resumed from suspend.
    Resuming,

    /// The system is about to shut down or reboot.
    ShuttingDown(LogindDelayHold),

    /// A previously requested shutdown or reboot was cancelled.
    ShutdownCancelled,
}

#[derive(Debug, Clone, Copy)]
enum InhibitorKind {
    Sleep,
    Shutdown,
}

impl InhibitorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Cloneable handle for an active logind delay inhibitor.
///
/// Releasing any clone drops the underlying inhibitor FD. The operation is
/// idempotent because all clones share the same slot.
#[derive(Debug, Clone)]
pub struct LogindDelayHold {
    inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
    label: &'static str,
}

impl LogindDelayHold {
    fn new(inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>, label: &'static str) -> Self {
        Self {
            inhibitor_fd,
            label,
        }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test(label: &'static str) -> Self {
        Self::new(Arc::new(Mutex::new(None)), label)
    }

    pub fn release(&self) {
        let Ok(mut guard) = self.inhibitor_fd.lock() else {
            warn!(
                "Failed to release {} inhibitor because the inhibitor state lock is poisoned",
                self.label
            );
            return;
        };

        if guard.take().is_some() {
            info!("Released {} inhibitor", self.label);
        }
    }
}

/// Monitors systemd-logind for power transition signals.
///
/// Acquires a sleep inhibitor lock (delay mode) so we get time to
/// clean up before the system actually suspends.
pub struct PowerMonitor {
    system_conn: Connection,
    power_tx: mpsc::Sender<PowerEvent>,
    /// Held inhibitor FD. Dropping releases the lock.
    sleep_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
    /// Held shutdown inhibitor FD. Dropping releases the lock.
    shutdown_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
}

impl PowerMonitor {
    /// Create a new power monitor.
    ///
    /// Returns the monitor and a receiver for power events.
    pub async fn new(
        system_conn: Connection,
    ) -> Result<(Self, mpsc::Receiver<PowerEvent>), crate::error::DbusError> {
        let (power_tx, power_rx) = mpsc::channel(8);

        Ok((Self::with_sender(system_conn, power_tx), power_rx))
    }

    fn with_sender(system_conn: Connection, power_tx: mpsc::Sender<PowerEvent>) -> Self {
        Self::with_inhibitors(
            system_conn,
            power_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
        )
    }

    fn with_inhibitors(
        system_conn: Connection,
        power_tx: mpsc::Sender<PowerEvent>,
        sleep_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
        shutdown_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
    ) -> Self {
        Self {
            system_conn,
            power_tx,
            sleep_inhibitor_fd,
            shutdown_inhibitor_fd,
        }
    }

    /// Run the power monitoring loop.
    ///
    /// This listens for logind prepare signals
    /// and sends `PowerEvent`s to the orchestrator.
    pub async fn run(self) -> Result<(), crate::error::DbusError> {
        info!("Starting power monitor");

        self.acquire_inhibitor(InhibitorKind::Sleep, "Publish status before sleep")
            .await?;
        self.acquire_inhibitor(InhibitorKind::Shutdown, "Publish status before shutdown")
            .await?;

        let proxy: zbus::Proxy = zbus::proxy::Builder::new(&self.system_conn)
            .destination("org.freedesktop.login1")?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .build()
            .await?;

        let mut sleep_stream = proxy.receive_signal("PrepareForSleep").await?;
        let mut shutdown_stream = proxy.receive_signal("PrepareForShutdown").await?;
        let mut shutdown_prepare_active = false;

        loop {
            tokio::select! {
                signal = sleep_stream.next() => {
                    let Some(signal) = signal else {
                        warn!("Power monitor sleep signal stream ended");
                        return Ok(());
                    };

                    let body: zbus::message::Body = signal.body();
                    let suspending: bool = match body.deserialize() {
                        Ok(val) => val,
                        Err(e) => {
                            error!("Failed to deserialize PrepareForSleep signal: {e}");
                            continue;
                        }
                    };

                    self.handle_prepare_for_sleep(suspending).await?;
                }

                signal = shutdown_stream.next() => {
                    let Some(signal) = signal else {
                        warn!("Power monitor shutdown signal stream ended");
                        return Ok(());
                    };

                    let body: zbus::message::Body = signal.body();
                    let shutting_down: bool = match body.deserialize() {
                        Ok(val) => val,
                        Err(e) => {
                            error!("Failed to deserialize PrepareForShutdown signal: {e}");
                            continue;
                        }
                    };

                    if shutting_down {
                        shutdown_prepare_active = true;
                        self.handle_prepare_for_shutdown(true).await?;
                    } else if shutdown_prepare_active {
                        self.handle_prepare_for_shutdown(false).await?;
                        self.wait_for_inhibitor_release(InhibitorKind::Shutdown).await?;
                        self.acquire_inhibitor(InhibitorKind::Shutdown, "Publish status before shutdown")
                            .await?;
                        shutdown_prepare_active = false;
                    } else {
                        info!("System shutdown preparation was cancelled without an active shutdown hold");
                    }
                }
            };
        }
    }

    async fn handle_prepare_for_sleep(
        &self,
        suspending: bool,
    ) -> Result<(), crate::error::DbusError> {
        emit_prepare_for_sleep_event(
            &self.power_tx,
            self.delay_hold(InhibitorKind::Sleep),
            suspending,
        )
        .await;

        if !suspending {
            self.acquire_inhibitor(InhibitorKind::Sleep, "Publish status before sleep")
                .await?;
        }

        Ok(())
    }

    async fn handle_prepare_for_shutdown(
        &self,
        shutting_down: bool,
    ) -> Result<(), crate::error::DbusError> {
        emit_prepare_for_shutdown_event(
            &self.power_tx,
            self.delay_hold(InhibitorKind::Shutdown),
            shutting_down,
        )
        .await;

        Ok(())
    }

    /// Acquire a delay-mode inhibitor from logind.
    ///
    /// The returned FD is stored in the relevant slot. The lock is held until
    /// the corresponding `LogindDelayHold` is released or the process exits.
    async fn acquire_inhibitor(
        &self,
        kind: InhibitorKind,
        reason: &str,
    ) -> Result<(), crate::error::DbusError> {
        let proxy: zbus::Proxy = zbus::proxy::Builder::new(&self.system_conn)
            .destination("org.freedesktop.login1")?
            .path("/org/freedesktop/login1")?
            .interface("org.freedesktop.login1.Manager")?
            .build()
            .await?;

        let fd: OwnedFd = proxy
            .call("Inhibit", &(kind.as_str(), "hars-imp", reason, "delay"))
            .await?;

        store_inhibitor_fd(self.inhibitor_slot(kind), kind.as_str(), fd)?;

        info!("Acquired {} inhibitor (delay mode)", kind.as_str());
        Ok(())
    }

    fn inhibitor_slot(&self, kind: InhibitorKind) -> &Arc<Mutex<Option<OwnedFd>>> {
        match kind {
            InhibitorKind::Sleep => &self.sleep_inhibitor_fd,
            InhibitorKind::Shutdown => &self.shutdown_inhibitor_fd,
        }
    }

    /// Build a handle that can release the currently held inhibitor.
    fn delay_hold(&self, kind: InhibitorKind) -> LogindDelayHold {
        LogindDelayHold::new(Arc::clone(self.inhibitor_slot(kind)), kind.as_str())
    }

    async fn wait_for_inhibitor_release(
        &self,
        kind: InhibitorKind,
    ) -> Result<(), crate::error::DbusError> {
        loop {
            let released = {
                let guard = self.inhibitor_slot(kind).lock().map_err(|_| {
                    crate::error::DbusError::InhibitorState(format!(
                        "{} inhibitor state lock is poisoned",
                        kind.as_str()
                    ))
                })?;
                guard.is_none()
            };

            if released {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn store_inhibitor_fd(
    slot: &Arc<Mutex<Option<OwnedFd>>>,
    label: &str,
    fd: OwnedFd,
) -> Result<(), crate::error::DbusError> {
    let mut guard = slot.lock().map_err(|_| {
        crate::error::DbusError::InhibitorState(format!("{label} inhibitor state lock is poisoned"))
    })?;
    *guard = Some(fd);
    Ok(())
}

async fn emit_prepare_for_sleep_event(
    power_tx: &mpsc::Sender<PowerEvent>,
    hold: LogindDelayHold,
    suspending: bool,
) {
    if suspending {
        info!("System preparing to suspend");
        send_power_event(power_tx, PowerEvent::Suspending(hold)).await;
    } else {
        info!("System resumed from suspend");
        let _ = power_tx.send(PowerEvent::Resuming).await;
    }
}

async fn emit_prepare_for_shutdown_event(
    power_tx: &mpsc::Sender<PowerEvent>,
    hold: LogindDelayHold,
    shutting_down: bool,
) {
    if shutting_down {
        info!("System preparing to shut down");
        send_power_event(power_tx, PowerEvent::ShuttingDown(hold)).await;
    } else {
        info!("System shutdown preparation was cancelled");
        let _ = power_tx.send(PowerEvent::ShutdownCancelled).await;
    }
}

async fn send_power_event(power_tx: &mpsc::Sender<PowerEvent>, event: PowerEvent) {
    let release_on_failure = match &event {
        PowerEvent::Suspending(hold) | PowerEvent::ShuttingDown(hold) => Some(hold.clone()),
        PowerEvent::Resuming | PowerEvent::ShutdownCancelled => None,
    };

    if power_tx.send(event).await.is_err() {
        if let Some(hold) = release_on_failure {
            hold.release();
        }
    }
}

/// Handle for the supervised power monitor task.
pub struct PowerMonitorSupervisor {
    receiver: Option<mpsc::Receiver<PowerEvent>>,
    shutdown_hold: LogindDelayHold,
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl PowerMonitorSupervisor {
    /// Spawn a supervised power monitor loop.
    pub fn spawn() -> Self {
        let (power_tx, receiver) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let sleep_inhibitor_fd = Arc::new(Mutex::new(None));
        let shutdown_inhibitor_fd = Arc::new(Mutex::new(None));
        let shutdown_hold = LogindDelayHold::new(
            Arc::clone(&shutdown_inhibitor_fd),
            InhibitorKind::Shutdown.as_str(),
        );
        let handle = tokio::spawn(supervise_power_monitor(
            power_tx,
            shutdown.clone(),
            Duration::from_secs(1),
            Duration::from_secs(60),
            move |power_tx| {
                let sleep_inhibitor_fd = Arc::clone(&sleep_inhibitor_fd);
                let shutdown_inhibitor_fd = Arc::clone(&shutdown_inhibitor_fd);
                async move {
                    run_power_monitor_once(power_tx, sleep_inhibitor_fd, shutdown_inhibitor_fd)
                        .await
                }
            },
        ));

        Self {
            receiver: Some(receiver),
            shutdown_hold,
            shutdown,
            handle,
        }
    }

    /// Take the single power-event receiver owned by this supervisor.
    pub fn take_receiver(&mut self) -> mpsc::Receiver<PowerEvent> {
        self.receiver
            .take()
            .expect("power event receiver should only be taken once")
    }

    /// Build a handle that releases the process-wide shutdown inhibitor.
    pub fn shutdown_delay_hold(&self) -> LogindDelayHold {
        self.shutdown_hold.clone()
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
    power_tx: mpsc::Sender<PowerEvent>,
    sleep_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
    shutdown_inhibitor_fd: Arc<Mutex<Option<OwnedFd>>>,
) -> Result<(), crate::error::DbusError> {
    let conn = crate::dbus::client::system_connection().await?;
    info!("D-Bus system connection established for power monitoring");
    let monitor =
        PowerMonitor::with_inhibitors(conn, power_tx, sleep_inhibitor_fd, shutdown_inhibitor_fd);
    monitor.run().await
}

async fn supervise_power_monitor<F, Fut>(
    power_tx: mpsc::Sender<PowerEvent>,
    shutdown: CancellationToken,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut run_once: F,
) where
    F: FnMut(mpsc::Sender<PowerEvent>) -> Fut,
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
    use std::fs::File;
    use std::os::fd::OwnedFd as StdOwnedFd;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn held_test_slot() -> Arc<Mutex<Option<OwnedFd>>> {
        let file = File::open("/dev/null").expect("open /dev/null");
        let fd: StdOwnedFd = file.into();
        Arc::new(Mutex::new(Some(OwnedFd::from(fd))))
    }

    fn empty_test_hold(label: &'static str) -> LogindDelayHold {
        LogindDelayHold::empty_for_test(label)
    }

    fn test_fd() -> OwnedFd {
        let file = File::open("/dev/null").expect("open /dev/null");
        let fd: StdOwnedFd = file.into();
        OwnedFd::from(fd)
    }

    #[test]
    fn delay_hold_release_is_idempotent() {
        let slot = held_test_slot();
        let hold = LogindDelayHold::new(Arc::clone(&slot), "shutdown");

        assert!(slot.lock().expect("lock slot").is_some());

        hold.release();
        assert!(slot.lock().expect("lock slot").is_none());

        hold.release();
        assert!(slot.lock().expect("lock slot").is_none());
    }

    #[test]
    fn delay_hold_release_handles_poisoned_lock() {
        let slot: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(None));
        let slot_for_thread = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _guard = slot_for_thread.lock().expect("lock slot");
            panic!("poison slot");
        })
        .join();
        let hold = LogindDelayHold::new(slot, "shutdown");

        hold.release();
    }

    #[test]
    fn store_inhibitor_fd_errors_when_lock_is_poisoned() {
        let slot: Arc<Mutex<Option<OwnedFd>>> = Arc::new(Mutex::new(None));
        let slot_for_thread = Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _guard = slot_for_thread.lock().expect("lock slot");
            panic!("poison slot");
        })
        .join();

        let err = store_inhibitor_fd(&slot, "shutdown", test_fd()).expect_err("poisoned lock");

        assert!(err.to_string().contains("shutdown inhibitor state lock"));
    }

    #[tokio::test]
    async fn prepare_for_shutdown_true_emits_shutdown_event() {
        let (tx, mut rx) = mpsc::channel(1);
        let hold = empty_test_hold("shutdown");

        emit_prepare_for_shutdown_event(&tx, hold, true).await;

        let event = rx.recv().await.expect("shutdown event should be sent");
        assert!(matches!(event, PowerEvent::ShuttingDown(_)));
    }

    #[tokio::test]
    async fn prepare_for_shutdown_false_emits_cancellation_event() {
        let (tx, mut rx) = mpsc::channel(1);
        let hold = empty_test_hold("shutdown");

        emit_prepare_for_shutdown_event(&tx, hold, false).await;

        let event = rx
            .recv()
            .await
            .expect("shutdown cancellation should be sent");
        assert!(matches!(event, PowerEvent::ShutdownCancelled));
    }

    #[tokio::test]
    async fn prepare_for_sleep_true_emits_suspend_event() {
        let (tx, mut rx) = mpsc::channel(1);
        let hold = empty_test_hold("sleep");

        emit_prepare_for_sleep_event(&tx, hold, true).await;

        let event = rx.recv().await.expect("suspend event should be sent");
        assert!(matches!(event, PowerEvent::Suspending(_)));
    }

    #[tokio::test]
    async fn prepare_for_sleep_false_emits_resume_event() {
        let (tx, mut rx) = mpsc::channel(1);
        let hold = empty_test_hold("sleep");

        emit_prepare_for_sleep_event(&tx, hold, false).await;

        let event = rx.recv().await.expect("resume event should be sent");
        assert!(matches!(event, PowerEvent::Resuming));
    }

    #[tokio::test]
    async fn failed_shutdown_event_send_releases_hold() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let slot = held_test_slot();
        let hold = LogindDelayHold::new(Arc::clone(&slot), "shutdown");

        emit_prepare_for_shutdown_event(&tx, hold, true).await;

        assert!(slot.lock().expect("lock slot").is_none());
    }

    #[tokio::test]
    async fn supervisor_restarts_after_monitor_exit() {
        let (tx, _rx) = mpsc::channel(8);
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
        let (tx, _rx) = mpsc::channel(8);
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
