use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::components::trait_def::{ActionMessage, OutboundMessage};

/// Convert a name to a URL/topic-safe slug.
pub fn slugify(name: &str) -> String {
    name.to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "_")
        .trim_matches('_')
        .to_string()
}

/// Execute a shell command asynchronously and return stdout.
///
/// Commands are killed if they do not complete within 30 seconds.
pub async fn execute_command(cmd: &str) -> Result<String, crate::error::ComponentError> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    execute_command_with_timeout(cmd, TIMEOUT).await
}

async fn execute_command_with_timeout(
    cmd: &str,
    timeout: Duration,
) -> Result<String, crate::error::ComponentError> {
    use std::process::Stdio;

    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|e| crate::error::ComponentError::CommandFailed(e.to_string()))?;

    let child_id = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| crate::error::ComponentError::CommandFailed("missing stdout pipe".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| crate::error::ComponentError::CommandFailed("missing stderr pipe".into()))?;

    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Err(crate::error::ComponentError::CommandFailed(e.to_string()));
        }
        Err(_) => {
            terminate_process_tree(child_id);
            if tokio::time::timeout(Duration::from_millis(500), child.wait())
                .await
                .is_err()
            {
                kill_process_tree(child_id);
                let _ = child.kill().await;
                let _ = child.wait().await;
            }

            return Err(crate::error::ComponentError::CommandFailed(format!(
                "Command timed out after {timeout:?}: {cmd}"
            )));
        }
    };

    let stdout = collect_pipe(stdout_task).await?;
    let stderr = collect_pipe(stderr_task).await?;

    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        Err(crate::error::ComponentError::CommandFailed(stderr))
    }
}

async fn collect_pipe(
    task: JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, crate::error::ComponentError> {
    task.await
        .map_err(|e| crate::error::ComponentError::CommandFailed(e.to_string()))?
        .map_err(|e| crate::error::ComponentError::CommandFailed(e.to_string()))
}

#[cfg(unix)]
fn terminate_process_tree(child_id: Option<u32>) {
    signal_process_group(child_id, libc::SIGTERM);
}

#[cfg(not(unix))]
fn terminate_process_tree(_child_id: Option<u32>) {}

#[cfg(unix)]
fn kill_process_tree(child_id: Option<u32>) {
    signal_process_group(child_id, libc::SIGKILL);
}

#[cfg(not(unix))]
fn kill_process_tree(_child_id: Option<u32>) {}

#[cfg(unix)]
fn signal_process_group(child_id: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = child_id {
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
}

/// Spawn a polling task that periodically invokes a callback and publishes
/// the resulting payload to `state_topic`.
///
/// The callback receives a `&mut S` (arbitrary state, e.g. `sysinfo::System`)
/// and returns the formatted payload string.
///
/// The task cancels cooperatively when `shutdown` is triggered.
pub fn spawn_polling_task<S, F>(
    sensor_name: &str,
    state_topic: String,
    interval: Duration,
    mut state: S,
    shutdown: CancellationToken,
    action_tx: mpsc::Sender<ActionMessage>,
    mut poll_fn: F,
) -> JoinHandle<()>
where
    S: Send + 'static,
    F: FnMut(&mut S) -> String + Send + 'static,
{
    let name = sensor_name.to_string();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    debug!("{name} polling task shutting down");
                    return;
                }
                _ = tokio::time::sleep(interval) => {
                    let payload = poll_fn(&mut state);
                    debug!("{name}: {payload}");

                    if let Err(e) = action_tx
                        .send(OutboundMessage::state(state_topic.clone(), payload))
                        .await
                    {
                        error!("Failed to send {name} update: {e}");
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    #[cfg(unix)]
    fn process_is_running(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello_world");
    }

    #[test]
    fn slugify_special_characters() {
        assert_eq!(slugify("CPU (%) Usage!"), "cpu_____usage");
    }

    #[test]
    fn slugify_already_slug() {
        assert_eq!(slugify("already_slug"), "already_slug");
    }

    #[test]
    fn slugify_uppercase() {
        assert_eq!(slugify("ALL_UPPER"), "all_upper");
    }

    #[test]
    fn slugify_leading_trailing_special() {
        // Leading and trailing non-alphanumeric chars become underscores, then trimmed
        assert_eq!(slugify("--test--"), "test");
        assert_eq!(slugify("  spaced  "), "spaced");
    }

    #[test]
    fn slugify_empty_string() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_numbers() {
        assert_eq!(slugify("Sensor 42"), "sensor_42");
    }

    #[test]
    fn slugify_consecutive_special() {
        // Multiple consecutive special chars all become underscores
        assert_eq!(slugify("a---b"), "a___b");
    }

    #[test]
    fn slugify_single_char() {
        assert_eq!(slugify("X"), "x");
        assert_eq!(slugify("-"), "");
    }

    #[test]
    fn slugify_mixed_case_and_symbols() {
        assert_eq!(slugify("Night Light / Toggle"), "night_light___toggle");
    }

    #[test]
    fn slugify_disk_usage_mount_point() {
        // Real usage from DiskUsageSensor
        // Trailing / becomes _ which is then trimmed by trim_matches('_')
        assert_eq!(slugify("Disk Usage /"), "disk_usage");
        assert_eq!(slugify("Disk Usage /home"), "disk_usage__home");
    }

    #[tokio::test]
    async fn execute_command_returns_stdout() {
        let output = execute_command_with_timeout("printf hello", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(output, "hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_timeout_kills_shell_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let cmd = format!("sleep 60 & echo $! > {}; wait", pid_file.display());

        let err = execute_command_with_timeout(&cmd, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));

        let pid = wait_for_pid_file(&pid_file).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_is_running(pid),
            "timed-out shell child process should be killed"
        );
    }

    #[cfg(unix)]
    async fn wait_for_pid_file(path: &Path) -> u32 {
        for _ in 0..20 {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if let Ok(pid) = contents.trim().parse() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("child pid file was not written");
    }
}
