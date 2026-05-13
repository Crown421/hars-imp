use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rumqttc::tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ClientConfig, RootCertStore,
};
use rumqttc::{
    AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS, TlsConfiguration, Transport,
};
use tokio::sync::mpsc;
use tokio::time::Sleep;
use tracing::{debug, error, info, warn};

use crate::components::trait_def::{ActionMessage, OutboundMessage};
use crate::config::Config;
use crate::error::MqttError;

const COMMAND_RESULT_BUFFER_CAP: usize = 256;

/// Wraps the rumqttc async client with reconnection-aware logic.
pub struct MqttClient {
    client: AsyncClient,
    eventloop: EventLoop,
}

/// Events emitted by the MQTT client loop to the orchestrator.
pub enum MqttEvent {
    /// An incoming publish message: (topic, payload).
    Message(String, String),

    /// The client has (re)connected to the broker.
    Connected,

    /// A connection error occurred; the client will retry.
    Disconnected(String),
}

impl MqttClient {
    /// Create a new MQTT client from config.
    pub fn new(config: &Config) -> Result<Self, Box<MqttError>> {
        let port = config.effective_mqtt_port();
        let mut opts = MqttOptions::new(&config.hostname, &config.mqtt_url, port);
        opts.set_credentials(&config.username, &config.password);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);

        // Configure TLS transport if enabled.
        if let Some(ref tls) = config.tls {
            let transport = build_tls_transport(tls)?;
            opts.set_transport(transport);
            info!("MQTT TLS enabled (port {port})");
        }

        // Set a last-will message so HA knows we're offline if we crash.
        let status_topic = config.status_topic();
        opts.set_last_will(rumqttc::LastWill::new(
            &status_topic,
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, eventloop) = AsyncClient::new(opts, 50);

        Ok(Self { client, eventloop })
    }

    /// Get a clone of the underlying async client for publishing.
    pub fn client(&self) -> AsyncClient {
        self.client.clone()
    }

    /// Run the MQTT event loop.
    ///
    /// This is a long-running task that:
    /// - Forwards incoming publishes to `event_tx`
    /// - Drains `action_rx` for outbound publishes
    /// - Signals connection/disconnection events
    /// - Uses exponential backoff on errors
    pub async fn run(
        mut self,
        event_tx: mpsc::Sender<MqttEvent>,
        mut action_rx: mpsc::Receiver<ActionMessage>,
    ) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(60);
        let mut reconnect_delay: Option<Pin<Box<Sleep>>> = None;
        let mut publish_buffer = PublishBuffer::new();
        let mut connected = false;
        let mut outbound_paused = false;
        let client = self.client.clone();

        loop {
            tokio::select! {
                // Poll the MQTT event loop
                poll_result = self.eventloop.poll(), if reconnect_delay.is_none() => {
                    outbound_paused = false;
                    match poll_result {
                        Ok(event) => {
                            backoff = Duration::from_secs(1); // reset on success
                            if is_connack(&event) {
                                connected = true;
                            }
                            handle_event(event, &event_tx).await;
                        }
                        Err(e) => {
                            connected = false;
                            let msg = format!("{e}");
                            error!("MQTT poll error: {msg}");
                            let _ = event_tx.send(MqttEvent::Disconnected(msg)).await;

                            reconnect_delay = Some(Box::pin(tokio::time::sleep(backoff)));
                            backoff = (backoff * 2).min(max_backoff);
                        }
                    }
                }

                _ = wait_for_reconnect_delay(&mut reconnect_delay), if reconnect_delay.is_some() => {
                    reconnect_delay = None;
                }

                // Drain outbound action messages
                Some(message) = action_rx.recv() => {
                    publish_buffer.push(message);
                    drain_available_actions(&mut action_rx, &mut publish_buffer);
                }

                _ = tokio::task::yield_now(), if connected && publish_buffer.has_pending() && !outbound_paused => {
                    let message = publish_buffer
                        .pop_next()
                        .expect("buffer should contain a pending outbound message");
                    if let Err(error) = try_publish_outbound(&client, message) {
                        let (message, e) = *error;
                        warn!("Failed to enqueue publish to {}: {e}", message.topic());
                        publish_buffer.push_front(message);
                        outbound_paused = true;
                    }
                }
            }
        }
    }
}

struct PublishBuffer {
    availability: CoalescedQueue,
    discovery: CoalescedQueue,
    command_results: VecDeque<OutboundMessage>,
    states: CoalescedQueue,
    command_result_cap: usize,
}

#[derive(Default)]
struct CoalescedQueue {
    order: VecDeque<String>,
    messages: HashMap<String, String>,
}

impl PublishBuffer {
    fn new() -> Self {
        Self::with_command_result_cap(COMMAND_RESULT_BUFFER_CAP)
    }

    fn with_command_result_cap(command_result_cap: usize) -> Self {
        Self {
            availability: CoalescedQueue::new(),
            discovery: CoalescedQueue::new(),
            command_results: VecDeque::new(),
            states: CoalescedQueue::new(),
            command_result_cap,
        }
    }

    fn push(&mut self, message: OutboundMessage) {
        match message {
            OutboundMessage::State { topic, payload } => {
                self.states.push_back(topic, payload);
            }
            OutboundMessage::Availability { topic, payload } => {
                self.availability.push_back(topic, payload);
            }
            OutboundMessage::Discovery { topic, payload } => {
                self.discovery.push_back(topic, payload);
            }
            message @ OutboundMessage::CommandResult { .. } => {
                if self.command_results.len() == self.command_result_cap {
                    if let Some(dropped) = self.command_results.pop_front() {
                        warn!(
                            "Dropping oldest buffered command result for '{}' because the outbound buffer is full",
                            dropped.topic()
                        );
                    }
                }
                self.command_results.push_back(message);
            }
        }
    }

    fn push_front(&mut self, message: OutboundMessage) {
        match message {
            OutboundMessage::State { topic, payload } => {
                self.states.push_front(topic, payload);
            }
            OutboundMessage::Availability { topic, payload } => {
                self.availability.push_front(topic, payload);
            }
            OutboundMessage::Discovery { topic, payload } => {
                self.discovery.push_front(topic, payload);
            }
            message @ OutboundMessage::CommandResult { .. } => {
                if self.command_results.len() == self.command_result_cap {
                    if let Some(dropped) = self.command_results.pop_back() {
                        warn!(
                            "Dropping newest buffered command result for '{}' to retry a failed publish",
                            dropped.topic()
                        );
                    }
                }
                self.command_results.push_front(message);
            }
        }
    }

    fn pop_next(&mut self) -> Option<OutboundMessage> {
        if let Some(message) = self.availability.pop_next(OutboundMessage::availability) {
            return Some(message);
        }

        if let Some(message) = self.discovery.pop_next(OutboundMessage::discovery) {
            return Some(message);
        }

        if let Some(message) = self.command_results.pop_front() {
            return Some(message);
        }

        self.states.pop_next(OutboundMessage::state)
    }

    fn has_pending(&self) -> bool {
        self.availability.has_pending()
            || self.discovery.has_pending()
            || !self.command_results.is_empty()
            || self.states.has_pending()
    }
}

impl CoalescedQueue {
    fn new() -> Self {
        Self::default()
    }

    fn push_back(&mut self, topic: String, payload: String) {
        if !self.messages.contains_key(&topic) {
            self.order.push_back(topic.clone());
        }
        self.messages.insert(topic, payload);
    }

    fn push_front(&mut self, topic: String, payload: String) {
        if !self.messages.contains_key(&topic) {
            self.order.push_front(topic.clone());
        }
        self.messages.insert(topic, payload);
    }

    fn pop_next(
        &mut self,
        build: impl Fn(String, String) -> OutboundMessage,
    ) -> Option<OutboundMessage> {
        while let Some(topic) = self.order.pop_front() {
            if let Some(payload) = self.messages.remove(&topic) {
                return Some(build(topic, payload));
            }
        }

        None
    }

    fn has_pending(&self) -> bool {
        !self.order.is_empty()
    }
}

fn drain_available_actions(
    action_rx: &mut mpsc::Receiver<ActionMessage>,
    publish_buffer: &mut PublishBuffer,
) {
    while let Ok(message) = action_rx.try_recv() {
        publish_buffer.push(message);
    }
}

fn try_publish_outbound(
    client: &AsyncClient,
    message: OutboundMessage,
) -> Result<(), Box<(OutboundMessage, rumqttc::ClientError)>> {
    let topic = message.topic().to_string();
    let retain = message.retain();
    let payload = message.payload().as_bytes().to_vec();

    client
        .try_publish(&topic, QoS::AtLeastOnce, retain, payload)
        .map_err(|e| Box::new((message, e)))
}

async fn wait_for_reconnect_delay(delay: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(delay) = delay {
        delay.as_mut().await;
    }
}

fn is_connack(event: &Event) -> bool {
    matches!(event, Event::Incoming(Packet::ConnAck(_)))
}

async fn handle_event(event: Event, event_tx: &mpsc::Sender<MqttEvent>) {
    match event {
        Event::Incoming(Packet::ConnAck(_)) => {
            info!("MQTT connected");
            let _ = event_tx.send(MqttEvent::Connected).await;
        }
        Event::Incoming(Packet::Publish(publish)) => {
            let topic = publish.topic.clone();
            let payload = String::from_utf8_lossy(&publish.payload).to_string();
            debug!("MQTT message: {topic} = {payload}");
            let _ = event_tx.send(MqttEvent::Message(topic, payload)).await;
        }
        Event::Incoming(Packet::SubAck(_)) => {
            debug!("MQTT subscription acknowledged");
        }
        _ => {
            // Ignore other events (PingResp, PubAck, etc.)
        }
    }
}

/// Build a `rumqttc::Transport` from the user's TLS configuration.
fn build_tls_transport(tls: &crate::config::TlsConfig) -> Result<Transport, Box<MqttError>> {
    let root_store = build_root_store(tls)?;
    let config_builder = ClientConfig::builder().with_root_certificates(root_store);

    let client_config = match (&tls.client_cert, &tls.client_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert = std::fs::read(cert_path)
                .map_err(|e| tls_error(format!("Failed to read client cert '{cert_path}': {e}")))?;
            let key = std::fs::read(key_path)
                .map_err(|e| tls_error(format!("Failed to read client key '{key_path}': {e}")))?;

            let certs = parse_pem_certs(cert_path, cert)?;
            let key = parse_private_key(key_path, key)?;

            config_builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| tls_error(format!("Invalid TLS client authentication config: {e}")))?
        }
        _ => config_builder.with_no_client_auth(),
    };

    Ok(Transport::tls_with_config(TlsConfiguration::Rustls(
        Arc::new(client_config),
    )))
}

fn build_root_store(tls: &crate::config::TlsConfig) -> Result<RootCertStore, Box<MqttError>> {
    let mut roots = RootCertStore::empty();

    if let Some(path) = &tls.ca_file {
        let ca = std::fs::read(path)
            .map_err(|e| tls_error(format!("Failed to read CA file '{path}': {e}")))?;
        let certs = parse_pem_certs(path, ca)?;
        let (added, _) = roots.add_parsable_certificates(certs);
        if added == 0 {
            return Err(tls_error(format!(
                "No valid CA certificates found in '{path}'"
            )));
        }
    } else {
        let certs = rustls_native_certs::load_native_certs()
            .map_err(|e| tls_error(format!("Failed to load native root certificates: {e}")))?;
        let (added, _) = roots.add_parsable_certificates(certs);
        if added == 0 {
            return Err(tls_error("No native root certificates could be loaded"));
        }
    }

    Ok(roots)
}

fn parse_pem_certs(
    path: &str,
    bytes: Vec<u8>,
) -> Result<Vec<CertificateDer<'static>>, Box<MqttError>> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(Cursor::new(bytes)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_error(format!("Failed to parse certificates in '{path}': {e}")))?;

    if certs.is_empty() {
        return Err(tls_error(format!("No certificates found in '{path}'")));
    }

    Ok(certs)
}

fn parse_private_key(path: &str, bytes: Vec<u8>) -> Result<PrivateKeyDer<'static>, Box<MqttError>> {
    rustls_pemfile::private_key(&mut BufReader::new(Cursor::new(bytes)))
        .map_err(|e| tls_error(format!("Failed to parse private key '{path}': {e}")))?
        .ok_or_else(|| tls_error(format!("No private key found in '{path}'")))
}

fn tls_error(message: impl Into<String>) -> Box<MqttError> {
    Box::new(MqttError::Tls(message.into()))
}

/// Publish a message with retain flag.
pub async fn publish_retained(
    client: &AsyncClient,
    topic: &str,
    payload: &str,
) -> Result<(), MqttError> {
    client
        .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes())
        .await?;
    Ok(())
}

/// Subscribe to a list of topics.
pub async fn subscribe_topics(client: &AsyncClient, topics: &[String]) -> Result<(), MqttError> {
    for topic in topics {
        info!("Subscribing to: {topic}");
        client.subscribe(topic, QoS::AtLeastOnce).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_buffer_coalesces_state_by_topic() {
        let mut buffer = PublishBuffer::new();
        buffer.push(OutboundMessage::state("topic/a", "old"));
        buffer.push(OutboundMessage::state("topic/a", "new"));

        let message = buffer.pop_next().expect("state should remain pending");
        assert_eq!(message.topic(), "topic/a");
        assert_eq!(message.payload(), "new");
        assert!(buffer.pop_next().is_none());
    }

    #[test]
    fn publish_buffer_preserves_reliable_messages_before_state() {
        let mut buffer = PublishBuffer::new();
        buffer.push(OutboundMessage::state("state/topic", "ON"));
        buffer.push(OutboundMessage::availability("status/topic", "online"));
        buffer.push(OutboundMessage::discovery("discovery/topic", "{}"));

        let first = buffer
            .pop_next()
            .expect("availability should publish first");
        assert_eq!(first.topic(), "status/topic");
        assert!(first.retain());

        let second = buffer.pop_next().expect("discovery should publish second");
        assert_eq!(second.topic(), "discovery/topic");
        assert!(second.retain());

        let third = buffer.pop_next().expect("state should publish last");
        assert_eq!(third.topic(), "state/topic");
        assert!(!third.retain());
    }

    #[test]
    fn publish_buffer_coalesces_retained_messages_by_topic() {
        let mut buffer = PublishBuffer::new();
        buffer.push(OutboundMessage::availability("status/topic", "offline"));
        buffer.push(OutboundMessage::availability("status/topic", "online"));
        buffer.push(OutboundMessage::discovery("discovery/topic", "old"));
        buffer.push(OutboundMessage::discovery("discovery/topic", "new"));

        let first = buffer
            .pop_next()
            .expect("availability should remain pending");
        assert_eq!(first.topic(), "status/topic");
        assert_eq!(first.payload(), "online");
        assert!(first.retain());

        let second = buffer.pop_next().expect("discovery should remain pending");
        assert_eq!(second.topic(), "discovery/topic");
        assert_eq!(second.payload(), "new");
        assert!(second.retain());

        assert!(buffer.pop_next().is_none());
    }

    #[test]
    fn publish_buffer_caps_command_results_and_drops_oldest() {
        let mut buffer = PublishBuffer::with_command_result_cap(2);
        buffer.push(OutboundMessage::command_result("command/1", "one"));
        buffer.push(OutboundMessage::command_result("command/2", "two"));
        buffer.push(OutboundMessage::command_result("command/3", "three"));

        let first = buffer
            .pop_next()
            .expect("second command result should remain");
        assert_eq!(first.topic(), "command/2");
        assert_eq!(first.payload(), "two");

        let second = buffer
            .pop_next()
            .expect("third command result should remain");
        assert_eq!(second.topic(), "command/3");
        assert_eq!(second.payload(), "three");

        assert!(buffer.pop_next().is_none());
    }
}
