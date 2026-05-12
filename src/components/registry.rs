use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::trait_def::{ActionMessage, Component};

const DEFAULT_HANDLER_LIMIT: usize = 64;
const DEFAULT_ROUTE_QUEUE_CAPACITY: usize = 16;

/// Manages a collection of components and provides O(1) topic-based routing.
pub struct ComponentRegistry {
    /// Maximum number of inbound component handlers that may run concurrently.
    handler_permits: Arc<Semaphore>,

    /// Maximum number of pending messages per component/topic route.
    route_queue_capacity: usize,

    /// All registered components.
    components: Vec<Arc<dyn Component>>,

    /// Maps MQTT topic → component/topic route workers.
    topic_map: HashMap<String, Vec<RouteDispatch>>,
}

struct RouteDispatch {
    component_idx: usize,
    queue: Arc<RouteQueue>,
}

struct RouteQueue {
    sender: Mutex<Option<mpsc::Sender<InboundMessage>>>,
}

struct InboundMessage {
    topic: String,
    payload: String,
    action_tx: mpsc::Sender<ActionMessage>,
}

impl ComponentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_HANDLER_LIMIT, DEFAULT_ROUTE_QUEUE_CAPACITY)
    }

    /// Create a registry with a custom inbound handler concurrency limit.
    pub fn with_handler_limit(handler_limit: usize) -> Self {
        Self::with_limits(handler_limit, DEFAULT_ROUTE_QUEUE_CAPACITY)
    }

    /// Create a registry with custom inbound handler and route queue limits.
    pub fn with_limits(handler_limit: usize, route_queue_capacity: usize) -> Self {
        Self {
            handler_permits: Arc::new(Semaphore::new(handler_limit)),
            route_queue_capacity: route_queue_capacity.max(1),
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
                .or_default()
                .push(RouteDispatch {
                    component_idx: idx,
                    queue: Arc::new(RouteQueue::new()),
                });
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
            Some(routes) => routes
                .iter()
                .map(|route| Arc::clone(&self.components[route.component_idx]))
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
        let Some(routes) = self.topic_map.get(topic) else {
            return;
        };

        for route in routes {
            let component = Arc::clone(&self.components[route.component_idx]);
            let sender = route.queue.sender(
                Arc::clone(&component),
                Arc::clone(&self.handler_permits),
                self.route_queue_capacity,
            );
            let message = InboundMessage {
                topic: topic.to_string(),
                payload: payload.to_string(),
                action_tx: action_tx.clone(),
            };

            match sender.try_send(message) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        "Dropping inbound MQTT message for '{}' because its handler queue is full",
                        component.name()
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        "Dropping inbound MQTT message for '{}' because its handler queue is closed",
                        component.name()
                    );
                }
            }
        }
    }

    /// Call `on_resume` on all components.
    pub async fn notify_resume(&self, action_tx: &mpsc::Sender<ActionMessage>) {
        for component in &self.components {
            component.on_resume(action_tx).await;
        }
    }
}

impl RouteQueue {
    fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    fn sender(
        &self,
        component: Arc<dyn Component>,
        handler_permits: Arc<Semaphore>,
        capacity: usize,
    ) -> mpsc::Sender<InboundMessage> {
        let mut sender = self.sender.lock().expect("route queue lock poisoned");
        if let Some(sender) = sender.as_ref() {
            return sender.clone();
        }

        let (tx, rx) = mpsc::channel(capacity);
        tokio::spawn(route_worker(component, handler_permits, rx));
        *sender = Some(tx.clone());
        tx
    }
}

async fn route_worker(
    component: Arc<dyn Component>,
    handler_permits: Arc<Semaphore>,
    mut rx: mpsc::Receiver<InboundMessage>,
) {
    while let Some(message) = rx.recv().await {
        let permit = match Arc::clone(&handler_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    "Dropping inbound MQTT message for '{}' because handler concurrency limit is exhausted",
                    component.name()
                );
                continue;
            }
        };

        let _permit = permit;
        component
            .handle_message(&message.topic, &message.payload, &message.action_tx)
            .await;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    struct CountingSlowComponent {
        name: &'static str,
        started: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Component for CountingSlowComponent {
        fn name(&self) -> &str {
            self.name
        }

        fn discovery_component(&self) -> HomeAssistantComponent {
            HomeAssistantComponent {
                name: self.name.to_string(),
                unique_id: self.name.to_string(),
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
            self.started.fetch_add(1, Ordering::SeqCst);
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

    struct RecordingComponent {
        records: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Component for RecordingComponent {
        fn name(&self) -> &str {
            "recording"
        }

        fn discovery_component(&self) -> HomeAssistantComponent {
            HomeAssistantComponent {
                name: "recording".to_string(),
                unique_id: "recording".to_string(),
                component_type: ComponentType::Switch {
                    command_topic: "topic/a".to_string(),
                    state_topic: "topic/state".to_string(),
                },
            }
        }

        fn subscriptions(&self) -> Vec<String> {
            vec!["topic/a".to_string()]
        }

        async fn handle_message(
            &self,
            _topic: &str,
            payload: &str,
            _action_tx: &mpsc::Sender<ActionMessage>,
        ) {
            self.records.lock().await.push(payload.to_string());
            tokio::time::sleep(Duration::from_millis(25)).await;
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

    #[tokio::test]
    async fn route_message_preserves_order_for_same_component_topic() {
        let mut registry = ComponentRegistry::new();
        let records = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        registry.register(Arc::new(RecordingComponent {
            records: Arc::clone(&records),
        }));

        let (tx, _rx) = mpsc::channel(16);
        registry.route_message("topic/a", "ON", &tx).await;
        registry.route_message("topic/a", "OFF", &tx).await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if records.lock().await.len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both messages should be handled");

        assert_eq!(*records.lock().await, vec!["ON", "OFF"]);
    }

    #[tokio::test]
    async fn route_message_drops_when_handler_limit_is_exhausted() {
        let mut registry = ComponentRegistry::with_handler_limit(1);
        let started = Arc::new(AtomicUsize::new(0));

        registry.register(Arc::new(CountingSlowComponent {
            name: "slow-a",
            started: Arc::clone(&started),
        }));
        registry.register(Arc::new(CountingSlowComponent {
            name: "slow-b",
            started: Arc::clone(&started),
        }));

        let (tx, _rx) = mpsc::channel(16);
        registry.route_message("topic/a", "PRESS", &tx).await;

        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("one handler should start");

        assert_eq!(started.load(Ordering::SeqCst), 1);
    }
}
