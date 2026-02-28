use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::trait_def::{ActionMessage, Component};

/// Manages a collection of components and provides O(1) topic-based routing.
pub struct ComponentRegistry {
    /// All registered components.
    components: Vec<Arc<dyn Component>>,

    /// Maps MQTT topic → indices into `components` that subscribe to that topic.
    topic_map: HashMap<String, Vec<usize>>,
}

impl ComponentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            components: Vec::new(),
            topic_map: HashMap::new(),
        }
    }

    /// Register a component. Builds the topic routing index.
    pub fn register(&mut self, component: Arc<dyn Component>) {
        let idx = self.components.len();
        for topic in component.subscriptions() {
            self.topic_map
                .entry(topic)
                .or_insert_with(Vec::new)
                .push(idx);
        }
        self.components.push(component);
    }

    /// Get all registered components.
    pub fn components(&self) -> &[Arc<dyn Component>] {
        &self.components
    }

    /// Find components subscribed to a given topic.
    pub fn components_for_topic(&self, topic: &str) -> Vec<Arc<dyn Component>> {
        match self.topic_map.get(topic) {
            Some(indices) => indices
                .iter()
                .map(|&idx| Arc::clone(&self.components[idx]))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Collect all unique subscription topics from all components.
    pub fn all_subscriptions(&self) -> Vec<String> {
        self.topic_map.keys().cloned().collect()
    }

    /// Spawn polling tasks for all components that support it.
    ///
    /// Returns a vec of join handles for the spawned tasks.
    pub fn spawn_polling_tasks(
        &self,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        self.components
            .iter()
            .filter_map(|component| {
                Arc::clone(component).spawn_polling(action_tx.clone(), shutdown.clone())
            })
            .collect()
    }

    /// Route an inbound MQTT message to all matching components.
    pub async fn route_message(
        &self,
        topic: &str,
        payload: &str,
        action_tx: &mpsc::Sender<ActionMessage>,
    ) {
        let targets = self.components_for_topic(topic);
        for component in targets {
            component.handle_message(topic, payload, action_tx).await;
        }
    }

    /// Call `on_resume` on all components.
    pub async fn notify_resume(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        for component in &self.components {
            component.on_resume(action_tx).await;
        }
    }
}
