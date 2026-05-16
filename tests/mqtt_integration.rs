//! Integration tests using a real mosquitto MQTT broker.
//!
//! Each test starts its own ephemeral mosquitto instance on a random port,
//! so tests can run in parallel without port conflicts.
//!
//! Requirements:
//! - `mosquitto` binary on PATH
//! - `mosquitto_sub` binary on PATH (for message verification)

use std::io::ErrorKind;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tokio::time::timeout;

// Re-export crate types for integration testing.
use hars_imp::components::button::ButtonComponent;
use hars_imp::components::registry::ComponentRegistry;
use hars_imp::components::status::{StatusComponent, StatusValue};
use hars_imp::components::switch::SwitchComponent;
use hars_imp::components::trait_def::{Component, OutboundMessage};
use hars_imp::config::{ButtonConfig, Config, SwitchConfig};
use hars_imp::mqtt::client::{MqttClient, MqttEvent};
use hars_imp::mqtt::discovery::DeviceDiscoveryBuilder;
use hars_imp::util::helpers::slugify;

// ---------------------------------------------------------------------------
// Test infrastructure: ephemeral mosquitto broker
// ---------------------------------------------------------------------------

/// An ephemeral mosquitto broker for testing.
struct TestBroker {
    child: Option<Child>,
    port: u16,
    config_path: PathBuf,
    _config_dir: tempfile::TempDir,
}

impl TestBroker {
    /// Start a mosquitto broker on a random available port.
    fn start() -> Option<Self> {
        let port = find_free_port();
        let config_dir = tempfile::tempdir().expect("create temp dir");
        let config_path = config_dir.path().join("mosquitto.conf");

        let mut f = std::fs::File::create(&config_path).expect("create config file");
        writeln!(f, "listener {port}\nallow_anonymous true\nlog_dest stderr")
            .expect("write config");

        let child = match start_mosquitto(&config_path, port) {
            Ok(child) => child,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                eprintln!("Skipping MQTT integration test: mosquitto is not on PATH");
                return None;
            }
            Err(e) => panic!("Failed to start mosquitto: {e}"),
        };

        Some(Self {
            child: Some(child),
            port,
            config_path,
            _config_dir: config_dir,
        })
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn restart(&mut self) {
        self.stop();
        self.child = Some(
            start_mosquitto(&self.config_path, self.port)
                .expect("restart mosquitto on existing test port"),
        );
    }
}

macro_rules! broker_or_skip {
    () => {
        match TestBroker::start() {
            Some(broker) => broker,
            None => return,
        }
    };
}

impl Drop for TestBroker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_mosquitto(config_path: &Path, port: u16) -> std::io::Result<Child> {
    let child = Command::new("mosquitto")
        .arg("-c")
        .arg(config_path)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()?;

    // Wait for the broker to start accepting connections.
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("mosquitto did not start within 5s on port {port}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(child)
}

/// Find a free TCP port by binding to port 0.
fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind to free port");
    listener.local_addr().unwrap().port()
}

/// Create a minimal test Config pointing at the ephemeral broker.
fn test_config(port: u16) -> Config {
    Config {
        hostname: "integrationtest".to_string(),
        mqtt_url: "127.0.0.1".to_string(),
        mqtt_port: Some(port),
        username: String::new(),
        password: String::new(),
        log_level: "debug".to_string(),
        update_interval_secs: 60,
        button: vec![],
        switch: vec![],
        tls: None,
    }
}

/// Create a raw rumqttc async client/eventloop for a helper subscriber.
fn helper_mqtt_client(port: u16, client_id: &str) -> (AsyncClient, rumqttc::EventLoop) {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    opts.set_clean_session(true);
    AsyncClient::new(opts, 50)
}

async fn connected_helper_client(port: u16, client_id: &str) -> (AsyncClient, rumqttc::EventLoop) {
    let (client, mut eventloop) = helper_mqtt_client(port, client_id);
    wait_for_helper_connected(&mut eventloop).await;
    (client, eventloop)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test: MqttClient connects and emits a Connected event.
#[tokio::test]
async fn mqtt_client_connects_and_emits_connected_event() {
    let broker = broker_or_skip!();
    let config = test_config(broker.port());
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(50);
    let (_action_tx, action_rx) = mpsc::channel(50);

    // Spawn the MQTT event loop.
    tokio::spawn(mqtt_client.run(event_tx, action_rx));

    // Should receive a Connected event within 3 seconds.
    let event = timeout(Duration::from_secs(3), event_rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("channel closed");

    assert!(
        matches!(event, MqttEvent::Connected),
        "Expected Connected event"
    );
}

/// Test: Publishing a message via the action channel reaches an MQTT subscriber.
#[tokio::test]
async fn publish_via_action_channel_reaches_subscriber() {
    let broker = broker_or_skip!();
    let config = test_config(broker.port());
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(50);
    let (action_tx, action_rx) = mpsc::channel(50);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));

    // Wait for connection.
    wait_for_connected(&mut event_rx).await;

    let (sub_client, mut sub_eventloop) = connected_helper_client(broker.port(), "test-sub").await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, "test/topic").await;

    // Publish via the action channel.
    action_tx
        .send(OutboundMessage::command_result(
            "test/topic",
            "hello integration",
        ))
        .await
        .expect("send action");

    let received = wait_for_publish(&mut sub_eventloop, None, Duration::from_secs(3)).await;

    assert_eq!(received.0, "test/topic");
    assert_eq!(received.1, "hello integration");
}

/// Test: An inbound message published to a subscribed topic is forwarded as MqttEvent::Message.
#[tokio::test]
async fn inbound_message_forwarded_as_event() {
    let broker = broker_or_skip!();
    let config = test_config(broker.port());
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(50);
    let (_action_tx, action_rx) = mpsc::channel(50);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));

    // Wait for connection.
    wait_for_connected(&mut event_rx).await;

    // Subscribe to a topic.
    async_client
        .subscribe("inbound/test", QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    // Give time for the subscription to take effect.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (pub_client, mut pub_eventloop) = connected_helper_client(broker.port(), "test-pub").await;
    publish_and_flush(
        &pub_client,
        &mut pub_eventloop,
        "inbound/test",
        false,
        b"payload123",
    )
    .await;

    let event = wait_for_mqtt_message(&mut event_rx, None, Duration::from_secs(3)).await;
    assert_eq!(event.0, "inbound/test");
    assert_eq!(event.1, "payload123");
}

/// Test: MqttClient reconnects after broker interruption and can publish again.
#[tokio::test]
async fn broker_restart_recovers_publish_path() {
    let mut broker = broker_or_skip!();
    let config = test_config(broker.port());
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    broker.stop();
    wait_for_disconnected(&mut event_rx).await;
    broker.restart();
    wait_for_connected(&mut event_rx).await;

    let (sub_client, mut sub_eventloop) =
        connected_helper_client(broker.port(), "restart-sub").await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, "restart/test").await;

    action_tx
        .send(OutboundMessage::command_result(
            "restart/test",
            "after restart",
        ))
        .await
        .expect("send action after restart");

    let received = wait_for_publish(&mut sub_eventloop, None, Duration::from_secs(3)).await;

    assert_eq!(received.0, "restart/test");
    assert_eq!(received.1, "after restart");
}

/// Test: buffered publishes larger than rumqttc's request channel flush after reconnect.
#[tokio::test]
async fn broker_restart_flushes_large_buffer_without_hanging() {
    let mut broker = broker_or_skip!();
    let config = test_config(broker.port());
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(200);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    broker.stop();
    wait_for_disconnected(&mut event_rx).await;

    for i in 0..75 {
        action_tx
            .send(OutboundMessage::discovery(
                format!("buffered/{i}"),
                format!("payload-{i}"),
            ))
            .await
            .expect("send buffered discovery");
    }

    broker.restart();
    wait_for_connected(&mut event_rx).await;

    let (sub_client, mut sub_eventloop) =
        connected_helper_client(broker.port(), "buffered-sub").await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, "buffered/74").await;

    let received = wait_for_publish(&mut sub_eventloop, None, Duration::from_secs(5)).await;

    assert_eq!(received.0, "buffered/74");
    assert_eq!(received.1, "payload-74");
}

/// Test: startup publishes availability online and retained user-visible status.
#[tokio::test]
async fn startup_publishes_availability_and_status_sensor() {
    let broker = broker_or_skip!();
    let config = test_config(broker.port());
    let status = Arc::new(StatusComponent::new(&config.hostname));

    let discovery = DeviceDiscoveryBuilder::new(&config)
        .add_components(status.discovery_components())
        .with_status_topic(config.status_topic())
        .build();
    let discovery_json = serde_json::to_string(&discovery).expect("serialize discovery");

    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    let (sub_client, mut sub_eventloop) =
        connected_helper_client(broker.port(), "startup-status-sub").await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, &config.discovery_topic()).await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, &config.status_topic()).await;
    subscribe_and_wait(
        &sub_client,
        &mut sub_eventloop,
        &StatusComponent::state_topic_for(&config.hostname),
    )
    .await;

    async_client
        .publish(
            &config.discovery_topic(),
            QoS::AtLeastOnce,
            true,
            discovery_json.as_bytes(),
        )
        .await
        .expect("publish discovery");
    async_client
        .publish(&config.status_topic(), QoS::AtLeastOnce, true, b"online")
        .await
        .expect("publish availability");

    status.on_resume(&action_tx).await;

    let (_, discovery_received) = wait_for_publish(
        &mut sub_eventloop,
        Some(&config.discovery_topic()),
        Duration::from_secs(3),
    )
    .await;
    let discovery_parsed: serde_json::Value =
        serde_json::from_str(&discovery_received).expect("parse discovery");
    assert!(discovery_parsed["cmps"]
        .get("integrationtest_status")
        .is_some());

    let (_, availability_payload) = wait_for_publish(
        &mut sub_eventloop,
        Some(&config.status_topic()),
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(availability_payload, "online");

    let (_, status_payload) = wait_for_publish(
        &mut sub_eventloop,
        Some(&StatusComponent::state_topic_for(&config.hostname)),
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(status_payload, StatusComponent::payload(StatusValue::On));
}

/// Test: Full component lifecycle — register components, build discovery JSON,
/// connect to broker, publish discovery, subscribe, and route an inbound message.
#[tokio::test]
async fn full_component_lifecycle() {
    let broker = broker_or_skip!();
    let port = broker.port();
    let mut config = test_config(port);

    // Add a button component.
    config.button.push(ButtonConfig {
        name: "Test Lock".to_string(),
        exec: "echo locked".to_string(),
    });

    // Add a switch component.
    config.switch.push(SwitchConfig {
        name: "Test Switch".to_string(),
        exec: Some("true".to_string()),
        dbus: None,
    });

    // --- Build registry ---
    let mut registry = ComponentRegistry::new();

    for btn_config in &config.button {
        let component = Arc::new(ButtonComponent::new(btn_config, &config.hostname));
        registry.register(component);
    }
    for sw_config in &config.switch {
        let component = Arc::new(SwitchComponent::new(sw_config, &config.hostname));
        registry.register(component);
    }

    assert_eq!(registry.components().len(), 2);
    assert_eq!(registry.all_subscriptions().len(), 2);

    // --- Build discovery JSON ---
    let components: Vec<_> = registry
        .components()
        .iter()
        .map(|c| {
            let key = slugify(c.name());
            let discovery = c.discovery_component();
            (key, discovery)
        })
        .collect();

    let discovery = DeviceDiscoveryBuilder::new(&config)
        .add_components(components)
        .with_status_topic(config.status_topic())
        .build();

    let discovery_json = serde_json::to_string(&discovery).expect("serialize discovery");

    // Verify discovery JSON contains expected keys.
    let parsed: serde_json::Value = serde_json::from_str(&discovery_json).unwrap();
    assert!(parsed.get("dev").is_some());
    assert!(parsed.get("cmps").is_some());
    let cmps = parsed["cmps"].as_object().unwrap();
    assert!(
        cmps.contains_key("test_lock"),
        "missing test_lock component"
    );
    assert!(
        cmps.contains_key("test_switch"),
        "missing test_switch component"
    );

    // --- Connect MQTT and publish discovery ---
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));

    // Wait for connection.
    wait_for_connected(&mut event_rx).await;

    // Publish discovery (retained).
    async_client
        .publish(
            &config.discovery_topic(),
            QoS::AtLeastOnce,
            true,
            discovery_json.as_bytes(),
        )
        .await
        .expect("publish discovery");

    // Publish online status.
    async_client
        .publish(&config.status_topic(), QoS::AtLeastOnce, true, b"online")
        .await
        .expect("publish status");

    // Subscribe to all component topics.
    let topics = registry.all_subscriptions();
    for topic in &topics {
        async_client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .expect("subscribe");
    }

    // Give subscriptions time to register.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (sub_client, mut sub_eventloop) = connected_helper_client(port, "verify-discovery").await;
    subscribe_and_wait(&sub_client, &mut sub_eventloop, &config.discovery_topic()).await;

    let (_, discovery_received) = wait_for_publish(
        &mut sub_eventloop,
        Some(&config.discovery_topic()),
        Duration::from_secs(3),
    )
    .await;

    let disc_parsed: serde_json::Value =
        serde_json::from_str(&discovery_received).expect("parse discovery JSON");
    assert_eq!(disc_parsed["dev"]["name"], "integrationtest");

    // --- Send an inbound message and verify routing ---
    // Publish a button press from the helper client.
    let button_topic = format!(
        "homeassistant/button/integrationtest/{}/set",
        slugify("Test Lock")
    );
    let (pub_client, mut pub_eventloop) = connected_helper_client(port, "send-press").await;
    publish_and_flush(
        &pub_client,
        &mut pub_eventloop,
        &button_topic,
        false,
        b"PRESS",
    )
    .await;

    let msg = wait_for_mqtt_message(&mut event_rx, None, Duration::from_secs(3)).await;

    assert_eq!(msg.0, button_topic);
    assert_eq!(msg.1, "PRESS");

    // Route the message through the registry.
    registry.route_message(&msg.0, &msg.1, &action_tx).await;
    // Button executes "echo locked" — no state message to verify, but it should not panic.
}

/// Test: Switch state round-trip — send ON command, verify state is published back.
#[tokio::test]
async fn switch_state_round_trip() {
    let broker = broker_or_skip!();
    let port = broker.port();
    let config = test_config(port);

    let sw_config = SwitchConfig {
        name: "Round Trip Switch".to_string(),
        exec: Some("true".to_string()),
        dbus: None,
    };
    let switch = Arc::new(SwitchComponent::new(&sw_config, &config.hostname));
    let state_topic = format!(
        "homeassistant/switch/{}/{}/state",
        config.hostname,
        slugify("Round Trip Switch")
    );

    // Set up MQTT client.
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    // Subscribe to the state topic so we can verify the published state.
    async_client
        .subscribe(&state_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe state topic");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Handle an ON message — the switch should publish state ON via action_tx.
    switch.handle_message("ignored", "ON", &action_tx).await;

    // The action_tx should have received a state publish message.
    // It goes through action_rx into the MQTT client, which publishes it.
    // Then we should see it back as an inbound message (since we're subscribed).
    let (_, state_msg) =
        wait_for_mqtt_message(&mut event_rx, Some(&state_topic), Duration::from_secs(3)).await;

    assert_eq!(state_msg, "ON");

    // Now send OFF.
    switch.handle_message("ignored", "OFF", &action_tx).await;

    let (_, state_msg) =
        wait_for_mqtt_message(&mut event_rx, Some(&state_topic), Duration::from_secs(3)).await;

    assert_eq!(state_msg, "OFF");
}

/// Test: on_resume re-publishes switch state correctly.
#[tokio::test]
async fn on_resume_publishes_switch_state() {
    let broker = broker_or_skip!();
    let config = test_config(broker.port());

    let sw_config = SwitchConfig {
        name: "Resume Switch".to_string(),
        exec: Some("true".to_string()),
        dbus: None,
    };
    let switch = Arc::new(SwitchComponent::new(&sw_config, &config.hostname));
    let state_topic = format!(
        "homeassistant/switch/{}/{}/state",
        config.hostname,
        slugify("Resume Switch")
    );

    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    async_client
        .subscribe(&state_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Turn switch ON first.
    switch.handle_message("ignored", "ON", &action_tx).await;

    // Consume the ON state publish.
    let _ = wait_for_mqtt_message(&mut event_rx, Some(&state_topic), Duration::from_secs(2)).await;

    // Now simulate resume — should re-publish ON.
    switch.on_resume(&action_tx).await;

    let (_, state_msg) =
        wait_for_mqtt_message(&mut event_rx, Some(&state_topic), Duration::from_secs(3)).await;

    assert_eq!(state_msg, "ON");
}

/// Test: ComponentRegistry correctly routes messages from MQTT to the right component.
#[tokio::test]
async fn registry_routes_mqtt_messages_to_components() {
    let broker = broker_or_skip!();
    let port = broker.port();
    let config = test_config(port);

    // Register a switch.
    let sw_config = SwitchConfig {
        name: "Routed Switch".to_string(),
        exec: Some("true".to_string()),
        dbus: None,
    };
    let switch = Arc::new(SwitchComponent::new(&sw_config, &config.hostname));
    let switch_cmd_topic = format!(
        "homeassistant/switch/{}/{}/set",
        config.hostname,
        slugify("Routed Switch")
    );
    let switch_state_topic = format!(
        "homeassistant/switch/{}/{}/state",
        config.hostname,
        slugify("Routed Switch")
    );

    let mut registry = ComponentRegistry::new();
    registry.register(switch);

    // Connect MQTT.
    let mqtt_client = MqttClient::new(&config).expect("create MqttClient");
    let async_client = mqtt_client.client();

    let (event_tx, mut event_rx) = mpsc::channel::<MqttEvent>(100);
    let (action_tx, action_rx) = mpsc::channel(100);

    tokio::spawn(mqtt_client.run(event_tx, action_rx));
    wait_for_connected(&mut event_rx).await;

    // Subscribe to the command and state topics.
    async_client
        .subscribe(&switch_cmd_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe cmd");
    async_client
        .subscribe(&switch_state_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe state");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (pub_client, mut pub_eventloop) = connected_helper_client(port, "route-pub").await;
    publish_and_flush(
        &pub_client,
        &mut pub_eventloop,
        &switch_cmd_topic,
        false,
        b"ON",
    )
    .await;

    // Wait for the inbound message.
    let (topic, payload) = wait_for_mqtt_message(
        &mut event_rx,
        Some(&switch_cmd_topic),
        Duration::from_secs(3),
    )
    .await;

    // Route through registry.
    registry.route_message(&topic, &payload, &action_tx).await;

    // The switch should publish its state via action_tx → MQTT.
    let (_, state_payload) = wait_for_mqtt_message(
        &mut event_rx,
        Some(&switch_state_topic),
        Duration::from_secs(3),
    )
    .await;

    assert_eq!(state_payload, "ON");
}

/// Test: Multiple retained messages persist and can be read by a new subscriber.
#[tokio::test]
async fn retained_messages_persist() {
    let broker = broker_or_skip!();
    let port = broker.port();

    // Publish some retained messages from client A.
    let (pub_client, mut pub_el) = connected_helper_client(port, "retain-pub").await;

    publish_and_flush(&pub_client, &mut pub_el, "retained/topic1", true, b"value1").await;
    publish_and_flush(&pub_client, &mut pub_el, "retained/topic2", true, b"value2").await;

    // New subscriber should receive retained messages.
    let (sub_client, mut sub_el) = connected_helper_client(port, "retain-sub").await;
    subscribe_and_wait(&sub_client, &mut sub_el, "retained/#").await;

    let mut received = std::collections::HashMap::new();
    let result = timeout(Duration::from_secs(3), async {
        loop {
            let event = sub_el.poll().await.expect("sub poll");
            if let Event::Incoming(Packet::Publish(publish)) = event {
                received.insert(
                    publish.topic.clone(),
                    String::from_utf8_lossy(&publish.payload).to_string(),
                );
                if received.len() >= 2 {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "Should have received both retained messages"
    );
    assert_eq!(received.get("retained/topic1").unwrap(), "value1");
    assert_eq!(received.get("retained/topic2").unwrap(), "value2");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wait for a Connected event, consuming any other events.
async fn wait_for_connected(event_rx: &mut mpsc::Receiver<MqttEvent>) {
    let connected = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(evt) = event_rx.recv().await {
                if matches!(evt, MqttEvent::Connected) {
                    return;
                }
            }
        }
    })
    .await;

    assert!(connected.is_ok(), "MqttClient did not connect within 5s");
}

async fn wait_for_disconnected(event_rx: &mut mpsc::Receiver<MqttEvent>) {
    let disconnected = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(evt) = event_rx.recv().await {
                if matches!(evt, MqttEvent::Disconnected(_)) {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        disconnected.is_ok(),
        "MqttClient did not report disconnect within 10s"
    );
}

async fn wait_for_helper_connected(eventloop: &mut rumqttc::EventLoop) {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = eventloop.poll().await.expect("helper poll");
            if matches!(event, Event::Incoming(Packet::ConnAck(_))) {
                return;
            }
        }
    })
    .await
    .expect("helper client did not connect within 5s");
}

async fn subscribe_and_wait(client: &AsyncClient, eventloop: &mut rumqttc::EventLoop, topic: &str) {
    client
        .subscribe(topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe");
    wait_for_suback(eventloop).await;
}

async fn wait_for_suback(eventloop: &mut rumqttc::EventLoop) {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = eventloop.poll().await.expect("helper poll");
            if matches!(event, Event::Incoming(Packet::SubAck(_))) {
                return;
            }
        }
    })
    .await
    .expect("helper subscription was not acknowledged within 5s");
}

async fn publish_and_flush(
    client: &AsyncClient,
    eventloop: &mut rumqttc::EventLoop,
    topic: &str,
    retain: bool,
    payload: &[u8],
) {
    client
        .publish(topic, QoS::AtLeastOnce, retain, payload)
        .await
        .expect("publish");
    let _ = timeout(Duration::from_millis(500), eventloop.poll()).await;
}

async fn wait_for_publish(
    eventloop: &mut rumqttc::EventLoop,
    expected_topic: Option<&str>,
    wait: Duration,
) -> (String, String) {
    timeout(wait, async {
        loop {
            let event = eventloop.poll().await.expect("helper poll");
            if let Event::Incoming(Packet::Publish(publish)) = event {
                if expected_topic.is_none_or(|topic| publish.topic == topic) {
                    return (
                        publish.topic,
                        String::from_utf8_lossy(&publish.payload).to_string(),
                    );
                }
            }
        }
    })
    .await
    .expect("timeout waiting for publish")
}

async fn wait_for_mqtt_message(
    event_rx: &mut mpsc::Receiver<MqttEvent>,
    expected_topic: Option<&str>,
    wait: Duration,
) -> (String, String) {
    timeout(wait, async {
        loop {
            if let Some(MqttEvent::Message(topic, payload)) = event_rx.recv().await {
                if expected_topic.is_none_or(|expected| topic == expected) {
                    return (topic, payload);
                }
            }
        }
    })
    .await
    .expect("timeout waiting for MQTT message")
}
