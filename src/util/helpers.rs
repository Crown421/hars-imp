use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::components::trait_def::ActionMessage;

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

    let result = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output(),
    )
    .await;

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(crate::error::ComponentError::CommandFailed(e.to_string()));
        }
        Err(_) => {
            return Err(crate::error::ComponentError::CommandFailed(format!(
                "Command timed out after {TIMEOUT:?}: {cmd}"
            )));
        }
    };

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(crate::error::ComponentError::CommandFailed(stderr))
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
                        .send((state_topic.clone(), payload))
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
}
