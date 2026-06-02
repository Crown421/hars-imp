# hars-imp — Implementation Progress

This document tracks implementation status for the `take-2` rewrite. Each section corresponds to a module or concern. Future agents should update this file as work progresses.

## Status Legend

- ✅ Complete — code written, compiles, basic functionality verified
- 🔧 In Progress — partially implemented
- ⬜ Not Started
- 🔲 Blocked — waiting on dependency

---

## Phase 1: Project Scaffold & Core Types

| Item | Status | File(s) | Notes |
|------|--------|---------|-------|
| `Cargo.toml` | ✅ | `Cargo.toml` | All dependencies declared |
| Error types | ✅ | `src/error.rs` | `thiserror`-based: Config, Mqtt, Dbus, Component variants |
| Config loading | ✅ | `src/config.rs` | TOML parsing, derived fields, debug/release paths |
| Component trait | ✅ | `src/components/trait_def.rs` | `async_trait`, ActionMessage, all methods defined |
| Component registry | ✅ | `src/components/registry.rs` | HashMap-based topic→component routing |
| Util modules | ✅ | `src/util/` | logging.rs, version.rs |
| Main entry point | ✅ | `src/main.rs` | Loads config, inits logging, runs orchestrator |

## Phase 2: MQTT Layer

| Item | Status | File(s) | Notes |
|------|--------|---------|-------|
| Discovery types | ✅ | `src/mqtt/discovery.rs` | `HomeAssistantComponent`, `ComponentType`, `DeviceDiscovery`, builder |
| MQTT client | ✅ | `src/mqtt/client.rs` | Connect, publish, subscribe; reconnect on `ConnAck` |

## Phase 3: D-Bus Layer

| Item | Status | File(s) | Notes |
|------|--------|---------|-------|
| D-Bus client factory | ✅ | `src/dbus/client.rs` | Shared `zbus::Connection` creation |
| Power monitor | ✅ | `src/dbus/power.rs` | `PrepareForSleep` signal listener, inhibitor acquire/release, broadcast channel |
| Notification sender | ✅ | `src/dbus/notifications.rs` | Desktop notifications via `org.freedesktop.Notifications` |

## Phase 4: Component Implementations

| Item | Status | File(s) | Notes |
|------|--------|---------|-------|
| ButtonComponent | ✅ | `src/components/button.rs` | Shell exec on `PRESS`, config-driven |
| SwitchComponent | ✅ | `src/components/switch.rs` | Shell exec or D-Bus method, state tracking |
| SystemMonitor | ✅ | `src/components/system_monitor.rs` | CPU, memory via sysinfo, polled |
| NotificationComponent | ✅ | `src/components/notification.rs` | MQTT JSON → D-Bus desktop notification |

## Phase 5: Orchestrator & Integration

| Item | Status | File(s) | Notes |
|------|--------|---------|-------|
| Orchestrator | ✅ | `src/orchestrator.rs` | Main select! loop, component lifecycle, suspend/resume |

## Phase 6: Compilation & Testing

| Item | Status | Notes |
|------|--------|-------|
| `cargo check` passes | ✅ | Zero warnings, all modules compile |
| `cargo build` passes | ✅ | Full debug build succeeds |
| Manual testing | ⬜ | Requires MQTT broker + HA instance |
| Unit tests | ✅ | 80 tests across core modules |
| Integration tests | ✅ | 10 tests with ephemeral mosquitto broker (`tests/mqtt_integration.rs`) |

---

## Architecture Decisions Made

1. **`&self` on `handle_message`** — Interior mutability (`Arc<Mutex<>>`) only where needed (switch state). Keeps trait object-safe.
2. **`CancellationToken`** over `AbortHandle` — Cooperative cancellation for polling tasks allows cleanup.
3. **`HashMap<String, Vec<Arc<dyn Component>>>`** for topic routing — O(1) lookup instead of linear scan.
4. **Single discovery message** — HA Device Discovery v2 format; one retained message for all entities.
5. **Explicit re-subscribe on ConnAck** — `clean_session = true`, we always re-subscribe rather than relying on broker state.
6. **Exponential backoff on MQTT errors** — 1s → 60s cap, reset on success.
7. **`thiserror` for module errors** — Typed errors for programmatic handling; no `Box<dyn Error>`.

## Known Bugs

| # | Severity | Location | Issue |
|---|----------|----------|-------|
| 1 | ~~**High**~~ | `orchestrator.rs` | ~~**CancellationToken resume bug.** Fixed: `Resuming` arm now creates a fresh `CancellationToken` instead of using `child_token()` on the cancelled parent.~~ ✅ |
| 2 | ~~**Medium**~~ | `dbus/power.rs` | ~~**Sleep inhibitor FD dropped immediately.** Fixed: FD is now stored in `Mutex<Option<OwnedFd>>` on `PowerMonitor` and explicitly released on suspend via `release_inhibitor()`.~~ ✅ |

## Known Future Work (Priority Order)

### P0 — Bug Fixes
- [x] Fix `CancellationToken` resume bug (see Known Bugs #1) — replaced `child_token()` with fresh `CancellationToken` on resume
- [x] Fix sleep inhibitor FD lifetime (see Known Bugs #2) — stored in `Mutex<Option<OwnedFd>>`, released on suspend
- [x] Fix Clippy warnings: `.or_insert_with(Vec::new)` → `.or_default()` in registry; boxed large `MqttError` variant in `AppError`

### P1 — Correctness & Robustness
- [x] Add SIGTERM handling (`tokio::signal::unix::signal(SignalKind::terminate())`) — added SIGTERM arm to `select!` loop alongside SIGINT
- [x] Cache D-Bus session connection — shared `OnceCell<Connection>` in `dbus/client.rs`; `notification.rs` and `switch.rs` now use cached connection
- [x] Command execution timeouts — `execute_command()` in `button.rs` now enforces a 30-second timeout via `tokio::time::timeout`
- [x] Switch state publish on startup/reconnect — reconnect synchronization calls `notify_resume()` to publish initial component states (including switch OFF)

### P2 — Code Quality & Refactoring
- [x] Move `slugify()` and `execute_command()` out of `button.rs` into `util/helpers.rs` — both are now shared by `button.rs`, `switch.rs`, and `orchestrator.rs`
- [x] Remove stale topic helpers on `Config` (`sensor_topic_base`, `button_topic_base`, `switch_topic_base`, `notify_topic`) — components build topics themselves, so these were unused dead code
- [x] Extract polling boilerplate — `spawn_polling_task()` helper in `util/helpers.rs` encapsulates the `select!`/`sleep`/`send` loop; CPU and memory sensors now use it via a closure

### P3 — Feature Completion
- [x] Disk usage sensor (`system_monitor.rs` — `DiskUsageSensor` monitoring `/` mount point, using `sysinfo::Disks`)
- [x] TLS support (`rumqttc` `Transport::Tls`) with optional `[tls]` config section (`ca_file`, `client_cert`, `client_key`); auto-detects port 8883 when TLS enabled
- [ ] Secret management (password not in plaintext config) — options proposed, awaiting decision

### P4 — Testing
- [x] Unit tests — config parsing/validation, `ComponentRegistry` routing, `handle_message` with known payloads, `slugify` edge cases, discovery JSON serialization round-trip, MQTT publish buffering
- [x] Integration tests with mosquitto broker — MQTT connect/pub/sub round-trip, discovery publishing, component lifecycle, switch state round-trip, resume state, retained messages, registry routing end-to-end, broker restart recovery
- [ ] Manual testing — requires MQTT broker + HA instance
