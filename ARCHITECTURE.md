# hars-imp — Architecture Document

## Overview

**hars-imp** is a Rust application that bridges a Linux PC to a Home Assistant instance via an MQTT broker. It exposes the machine as an HA device with sensors, buttons, switches, and notifications. It also listens to D-Bus signals (suspend/resume) and can execute shell commands or call D-Bus methods in response to HA actions.

## Design Goals

1. **Modularity & Extensibility** — The `Component` trait is the unit of extensibility. Adding a new entity type means implementing one trait in one file.
2. **Efficiency** — Tokio async runtime, zero-copy where possible, `Arc<dyn Component>` for shared ownership without cloning.
3. **Event-Driven** — Prefer signal/stream subscriptions over polling. Polling only for system metrics.
4. **Robustness** — Self-healing MQTT reconnection with exponential backoff. Survives suspend/resume cycles by re-establishing all connections.

## Module Map

```
src/
├── main.rs                     # Entry point: load config, init logging, run orchestrator
├── orchestrator.rs             # Central coordinator: spawns tasks, runs select! loop
├── config.rs                   # TOML config loading & validation
├── error.rs                    # Typed errors via thiserror
│
├── mqtt/
│   ├── mod.rs                  # Re-exports
│   ├── client.rs               # MqttClient: connect, reconnect, subscribe, publish
│   └── discovery.rs            # HA MQTT device discovery payload types
│
├── dbus/
│   ├── mod.rs                  # Re-exports
│   ├── client.rs               # Shared zbus::Connection factory
│   ├── power.rs                # Suspend/resume signal listener + inhibitor locks
│   └── notifications.rs        # Desktop notification sender via D-Bus
│
├── components/
│   ├── mod.rs                  # Re-exports
│   ├── trait_def.rs            # Component trait + outbound ActionMessage type
│   ├── registry.rs             # ComponentRegistry: topic→component routing
│   ├── button.rs               # ButtonComponent (config-driven, shell exec on PRESS)
│   ├── switch.rs               # SwitchComponent (shell exec or D-Bus method call)
│   ├── system_monitor.rs       # CPU, memory, disk sensors via sysinfo
│   └── notification.rs         # NotificationComponent (MQTT JSON → D-Bus notify)
│
└── util/
    ├── mod.rs                  # Re-exports
    ├── helpers.rs              # slugify(), execute_command(), spawn_polling_task()
    ├── logging.rs              # tracing-subscriber initialization
    └── version.rs              # Compile-time version info
```

## Key Abstractions

### Component Trait (`components/trait_def.rs`)

Every HA entity implements this trait:

```rust
#[async_trait]
pub trait Component: Send + Sync {
    /// Human-readable name, also used as the component key in discovery.
    fn name(&self) -> &str;

    /// Returns the HA discovery config fragment for this entity.
    fn discovery_component(&self) -> HomeAssistantComponent;

    /// MQTT topics this component wants to receive messages from.
    /// Defaults to an empty list for publish-only components.
    fn subscriptions(&self) -> Vec<String> {
        Vec::new()
    }

    /// Handle an inbound MQTT message. Use action_tx to publish responses.
    async fn handle_message(
        &self,
        topic: &str,
        payload: &str,
        action_tx: &mpsc::Sender<ActionMessage>,
    ) {
    }

    /// For polled components (sensors): spawn a background task.
    /// Returns None for event-only components.
    fn spawn_polling(
        self: Arc<Self>,
        action_tx: mpsc::Sender<ActionMessage>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        None
    }

    /// Called after resume from suspend. Re-publish current state.
    async fn on_resume(&self, action_tx: &mpsc::Sender<ActionMessage>) {}
}
```

### Channel Architecture

```
┌──────────────┐  action_tx   ┌──────────────┐  publish   ┌──────────────┐
│  Components  │ ───────────► │  MqttClient  │ ─────────► │  MQTT Broker │
│  (sensors,   │              │  event loop  │            │              │
│   handlers)  │ ◄─────────── │              │ ◄───────── │              │
└──────────────┘  MqttEvent   └──────────────┘  subscribe  └──────────────┘
        ▲                            ▲
        │ route_message              │ sync requests
        └──────────────┬─────────────┘
                       │
                ┌──────────────┐
                │ Orchestrator │ ◄── power_rx ── D-Bus Power Monitor
                └──────────────┘
```

- **ActionMessage** `OutboundMessage` — typed outbound MQTT publish request. Components send these.
- **MqttEvent** `{Connected, Message(String, String), Disconnected(String)}` — inbound MQTT events. Orchestrator routes `Message` variants to components.
- **PowerEvent** `{Suspending, Resuming}` — broadcast from D-Bus power monitor.

### Orchestrator (`orchestrator.rs`)

The orchestrator owns the main `tokio::select!` loop with three concerns:

1. **MQTT events** — schedules reconnect synchronization or routes incoming publishes to matching components
2. **Power events** — handles suspend/resume by stopping and restarting polling work and scheduling synchronization
3. **Shutdown signals** — SIGINT/SIGTERM → graceful cleanup

The `MqttClient` owns the MQTT poll loop, outbound action drain, reconnect backoff, and coalescing publish buffer.

### Discovery Protocol

Uses HA MQTT Device Discovery v2 (single device-based message):

1. At startup/reconnect, collect `discovery_component()` from every registered component
2. Assemble into a single `HomeAssistantDeviceDiscovery` with device info + origin + components map
3. Publish to `homeassistant/device/{hostname}/config` with `retain = true`, `QoS::AtLeastOnce`
4. Publish retained availability on the status topic so HA can track online/offline state

### MQTT Reconnection Strategy

- `clean_session = true` (stateless — we always re-subscribe)
- On `ConnAck` event: re-subscribe all topics, re-publish discovery
- On poll error: log, exponential backoff (1s → 2s → 4s → … → 60s cap), retry
- On successful poll: reset backoff to 1s

### Suspend/Resume Flow

```
PrepareForSleep(true) from logind
  → PowerEvent::Suspending via broadcast
  → Orchestrator: publish "offline", cancel sensor tasks
  → System suspends

PrepareForSleep(false) from logind
  → PowerEvent::Resuming via broadcast
  → Orchestrator: restart sensor tasks, request reconnect synchronization
  → Reconnect sync worker: re-publish discovery, re-subscribe topics,
    publish "online", re-publish current component state
```

## Configuration

TOML format at `$HOME/.config/hars-imp/config.toml` (release) or `./config.toml` (debug):

```toml
hostname = "my-pc"
mqtt_url = "192.168.1.100"
mqtt_port = 1883
username = "ha_user"
password = "ha_pass"
log_level = "info"
update_interval_secs = 60

[[button]]
name = "Suspend"
exec = "systemctl suspend"

[[switch]]
name = "Idle Inhibit"
dbus = { service = "org.example.Idle", path = "/", interface = "org.example.Idle", method = "SetInhibit" }

[[switch]]
name = "Test Switch"
exec = "echo switched"
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| `rumqttc` | Async MQTT 3.1.1 client |
| `tokio` | Async runtime (full features) |
| `zbus` | D-Bus client (async, logind signals) |
| `serde` + `serde_json` | Serialization |
| `toml` | Config file parsing |
| `sysinfo` | CPU, memory, disk metrics |
| `tracing` + `tracing-subscriber` | Structured logging |
| `thiserror` | Typed error definitions |
| `async-trait` | Async methods in traits |
| `tokio-util` | `CancellationToken` for task shutdown |
