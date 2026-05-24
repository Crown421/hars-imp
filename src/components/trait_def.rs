use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::mqtt::discovery::HomeAssistantComponent;

/// An outbound MQTT publish request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundMessage {
    /// Replaceable entity state. Multiple pending messages for the same topic may be coalesced.
    State { topic: String, payload: String },

    /// Replaceable retained entity state. Multiple pending messages for the same topic may be coalesced.
    RetainedState { topic: String, payload: String },

    /// Availability/status should be retained and should not be coalesced with state updates.
    Availability { topic: String, payload: String },

    /// Discovery payloads should be retained and preserved.
    Discovery { topic: String, payload: String },

    /// Non-state command response or ad hoc publish.
    CommandResult { topic: String, payload: String },
}

impl OutboundMessage {
    pub fn state(topic: impl Into<String>, payload: impl Into<String>) -> Self {
        Self::State {
            topic: topic.into(),
            payload: payload.into(),
        }
    }

    pub fn availability(topic: impl Into<String>, payload: impl Into<String>) -> Self {
        Self::Availability {
            topic: topic.into(),
            payload: payload.into(),
        }
    }

    pub fn retained_state(topic: impl Into<String>, payload: impl Into<String>) -> Self {
        Self::RetainedState {
            topic: topic.into(),
            payload: payload.into(),
        }
    }

    pub fn discovery(topic: impl Into<String>, payload: impl Into<String>) -> Self {
        Self::Discovery {
            topic: topic.into(),
            payload: payload.into(),
        }
    }

    pub fn command_result(topic: impl Into<String>, payload: impl Into<String>) -> Self {
        Self::CommandResult {
            topic: topic.into(),
            payload: payload.into(),
        }
    }

    pub fn topic(&self) -> &str {
        match self {
            Self::State { topic, .. }
            | Self::RetainedState { topic, .. }
            | Self::Availability { topic, .. }
            | Self::Discovery { topic, .. }
            | Self::CommandResult { topic, .. } => topic,
        }
    }

    pub fn payload(&self) -> &str {
        match self {
            Self::State { payload, .. }
            | Self::RetainedState { payload, .. }
            | Self::Availability { payload, .. }
            | Self::Discovery { payload, .. }
            | Self::CommandResult { payload, .. } => payload,
        }
    }

    pub fn retain(&self) -> bool {
        matches!(
            self,
            Self::RetainedState { .. } | Self::Availability { .. } | Self::Discovery { .. }
        )
    }
}

pub type ActionMessage = OutboundMessage;

/// The core abstraction for every Home Assistant entity.
///
/// Each component knows how to:
/// - Describe itself for HA MQTT discovery
/// - Declare which MQTT topics it needs
/// - Handle inbound MQTT messages
/// - Optionally run a polling loop (for sensors)
/// - Synchronize current state when requested by the orchestrator
#[async_trait]
pub trait Component: Send + Sync {
    /// Human-readable name, used as the component key in discovery.
    fn name(&self) -> &str;

    /// Returns the HA discovery configuration fragment for this entity.
    fn discovery_component(&self) -> HomeAssistantComponent;

    /// Returns all HA discovery entries contributed by this component.
    fn discovery_components(&self) -> Vec<(String, HomeAssistantComponent)> {
        vec![(
            crate::util::helpers::slugify(self.name()),
            self.discovery_component(),
        )]
    }

    /// MQTT topics this component wants to subscribe to.
    /// Return an empty vec for components that only publish.
    fn subscriptions(&self) -> Vec<String> {
        Vec::new()
    }

    /// Handle an inbound MQTT message on a subscribed topic.
    ///
    /// Use `action_tx` to send outbound MQTT messages (e.g. state updates).
    async fn handle_message(
        &self,
        _topic: &str,
        _payload: &str,
        _action_tx: &mpsc::Sender<ActionMessage>,
    ) {
    }

    /// Spawn a background polling task for this component.
    ///
    /// Returns `None` for event-only components (buttons, notifications).
    /// The task should respect the `shutdown` token for cooperative cancellation.
    fn spawn_polling(
        self: Arc<Self>,
        _action_tx: mpsc::Sender<ActionMessage>,
        _shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        None
    }

    /// Called when the orchestrator requests a component-wide state synchronization.
    ///
    /// Components should publish or reconcile their current state so downstream
    /// consumers observe the latest value after reconnect/startup or resume.
    async fn sync_state(&self, _action_tx: &mpsc::Sender<ActionMessage>) {}
}
