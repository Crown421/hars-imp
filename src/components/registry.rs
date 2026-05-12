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
            self.topic_map.entry(topic).or_default().push(idx);
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
            let topic = topic.to_string();
            let payload = payload.to_string();
            let action_tx = action_tx.clone();

            tokio::spawn(async move {
                component.handle_message(&topic, &payload, &action_tx).await;
            });
        }
    }

    /// Call `on_resume` on all components.
    pub async fn notify_resume(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        for component in &self.components {
            component.on_resume(action_tx).await;
        }
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};
    use std::time::Duration;

    /// A minimal mock component for registry tests.
    struct MockComponent {
        name: String,
        topics: Vec<String>,
    }

    impl MockComponent {
        fn new(name: &str, topics: Vec<&str>) -> Self {
            Self {
                name: name.to_string(),
                topics: topics.into_iter().map(String::from).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Component for MockComponent {
        fn name(&self) -> &str {
            &self.name
        }

        fn discovery_component(&self) -> HomeAssistantComponent {
            HomeAssistantComponent {
                name: self.name.clone(),
                unique_id: format!("mock_{}", self.name),
                component_type: ComponentType::Button {
                    command_topic: "unused".to_string(),
                },
            }
        }

        fn subscriptions(&self) -> Vec<String> {
            self.topics.clone()
        }

        async fn handle_message(
            &self,
            _topic: &str,
            _payload: &str,
            _action_tx: &mpsc::Sender<ActionMessage>,
        ) {
        }

        fn spawn_polling(
            self: Arc<Self>,
            _action_tx: mpsc::Sender<ActionMessage>,
            _shutdown: CancellationToken,
        ) -> Option<JoinHandle<()>> {
            None
        }

        async fn on_resume(&self, _action_tx: &mpsc::Sender<ActionMessage>) {}
    }

    struct SlowComponent;

    #[async_trait::async_trait]
    impl Component for SlowComponent {
        fn name(&self) -> &str {
            "slow"
        }

        fn discovery_component(&self) -> HomeAssistantComponent {
            HomeAssistantComponent {
                name: "slow".to_string(),
                unique_id: "slow".to_string(),
                component_type: ComponentType::Button {
                    command_topic: "topic/a".to_string(),
                },
            }
        }

        fn subscriptions(&self) -> Vec<String> {
            vec!["topic/a".to_string()]
        }

        async fn handle_message(
            &self,
            _topic: &str,
            _payload: &str,
            _action_tx: &mpsc::Sender<ActionMessage>,
        ) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        fn spawn_polling(
            self: Arc<Self>,
            _action_tx: mpsc::Sender<ActionMessage>,
            _shutdown: CancellationToken,
        ) -> Option<JoinHandle<()>> {
            None
        }

        async fn on_resume(&self, _action_tx: &mpsc::Sender<ActionMessage>) {}
    }

    #[test]
    fn empty_registry() {
        let registry = ComponentRegistry::new();
        assert!(registry.components().is_empty());
        assert!(registry.all_subscriptions().is_empty());
        assert!(registry.components_for_topic("any/topic").is_empty());
    }

    #[test]
    fn register_single_component() {
        let mut registry = ComponentRegistry::new();
        let c = Arc::new(MockComponent::new("btn", vec!["topic/a"]));
        registry.register(c);

        assert_eq!(registry.components().len(), 1);
        assert_eq!(registry.components()[0].name(), "btn");
    }

    #[test]
    fn route_to_correct_component() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new("btn1", vec!["topic/a"])));
        registry.register(Arc::new(MockComponent::new("btn2", vec!["topic/b"])));

        let matches_a = registry.components_for_topic("topic/a");
        assert_eq!(matches_a.len(), 1);
        assert_eq!(matches_a[0].name(), "btn1");

        let matches_b = registry.components_for_topic("topic/b");
        assert_eq!(matches_b.len(), 1);
        assert_eq!(matches_b[0].name(), "btn2");
    }

    #[test]
    fn no_match_returns_empty() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new("btn", vec!["topic/a"])));
        assert!(registry
            .components_for_topic("topic/nonexistent")
            .is_empty());
    }

    #[test]
    fn multiple_components_same_topic() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new("c1", vec!["shared/topic"])));
        registry.register(Arc::new(MockComponent::new("c2", vec!["shared/topic"])));

        let matches = registry.components_for_topic("shared/topic");
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"c1"));
        assert!(names.contains(&"c2"));
    }

    #[test]
    fn component_with_multiple_subscriptions() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new(
            "multi",
            vec!["t/1", "t/2", "t/3"],
        )));

        assert_eq!(registry.components_for_topic("t/1").len(), 1);
        assert_eq!(registry.components_for_topic("t/2").len(), 1);
        assert_eq!(registry.components_for_topic("t/3").len(), 1);
    }

    #[test]
    fn all_subscriptions_collects_unique_topics() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new(
            "c1",
            vec!["topic/a", "topic/b"],
        )));
        registry.register(Arc::new(MockComponent::new(
            "c2",
            vec!["topic/b", "topic/c"],
        )));

        let mut subs = registry.all_subscriptions();
        subs.sort();
        assert_eq!(subs, vec!["topic/a", "topic/b", "topic/c"]);
    }

    #[test]
    fn sensor_no_subscriptions() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new("sensor", vec![])));

        assert_eq!(registry.components().len(), 1);
        assert!(registry.all_subscriptions().is_empty());
    }

    #[tokio::test]
    async fn route_message_dispatches() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(MockComponent::new("btn", vec!["topic/a"])));

        let (tx, _rx) = mpsc::channel(16);
        // Should not panic — dispatches to the mock component
        registry.route_message("topic/a", "PRESS", &tx).await;
    }

    #[tokio::test]
    async fn route_message_does_not_wait_for_slow_handlers() {
        let mut registry = ComponentRegistry::new();
        registry.register(Arc::new(SlowComponent));

        let (tx, _rx) = mpsc::channel(16);
        let result = tokio::time::timeout(
            Duration::from_millis(50),
            registry.route_message("topic/a", "PRESS", &tx),
        )
        .await;

        assert!(
            result.is_ok(),
            "routing should not wait for handler completion"
        );
    }
}
