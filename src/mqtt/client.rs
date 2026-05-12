use std::io::{BufReader, Cursor};
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
use tracing::{debug, error, info, warn};

use crate::components::trait_def::ActionMessage;
use crate::config::Config;
use crate::error::MqttError;

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
        let client = self.client.clone();

        loop {
            tokio::select! {
                // Poll the MQTT event loop
                poll_result = self.eventloop.poll() => {
                    match poll_result {
                        Ok(event) => {
                            backoff = Duration::from_secs(1); // reset on success
                            handle_event(event, &event_tx).await;
                        }
                        Err(e) => {
                            let msg = format!("{e}");
                            error!("MQTT poll error: {msg}");
                            let _ = event_tx.send(MqttEvent::Disconnected(msg)).await;

                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(max_backoff);
                        }
                    }
                }

                // Drain outbound action messages
                Some((topic, payload)) = action_rx.recv() => {
                    let payload_bytes: &[u8] = payload.as_bytes();
                    if let Err(e) = client.publish(
                        &topic,
                        QoS::AtLeastOnce,
                        false,
                        payload_bytes,
                    ).await {
                        warn!("Failed to publish to {topic}: {e}");
                    }
                }
            }
        }
    }
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
