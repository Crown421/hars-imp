use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::mqtt::discovery::HomeAssistantComponent;

/// An outbound MQTT message: (topic, payload).
pub type ActionMessage = (String, String);

/// The core abstraction for every Home Assistant entity.
///
/// Each component knows how to:
/// - Describe itself for HA MQTT discovery
/// - Declare which MQTT topics it needs
/// - Handle inbound MQTT messages
/// - Optionally run a polling loop (for sensors)
/// - Re-publish state after a suspend/resume cycle
#[async_trait]
pub trait Component: Send + Sync {
    /// Human-readable name, used as the component key in discovery.
    fn name(&self) -> &str;

    /// Returns the HA discovery configuration fragment for this entity.
    fn discovery_component(&self) -> HomeAssistantComponent;

    /// MQTT topics this component wants to subscribe to.
    /// Return an empty vec for components that only publish.
    fn subscriptions(&self) -> Vec<String>;

    /// Handle an inbound MQTT message on a subscribed topic.
    ///
    /// Use `action_tx` to send outbound MQTT messages (e.g. state updates).
    async fn handle_message(
        &self,
        topic: &str,
        payload: &str,
        action_tx: &mpsc::Sender<ActionMessage>,
    );

    /// Spawn a background polling task for this component.
    ///
    /// Returns `None` for event-only components (buttons, notifications).
    /// The task should respect the `shutdown` token for cooperative cancellation.
    fn spawn_polling(
        self: Arc<Self>,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>>;

    /// Called after the system resumes from suspend.
    ///
    /// Components should re-publish their current state.
    async fn on_resume(&self, action_tx: &mpsc::Sender<ActionMessage>);
}
