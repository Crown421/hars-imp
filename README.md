# hars-imp

A minimal and efficient Rust daemon for interacting with Home Assistant via MQTT

## Features

- Reads configuration from a TOML file
- Connects to MQTT broker with authentication
- Subscribes to multiple topics
- Prints received messages with timestamps
- Automatic reconnection on connection failures
- **Home Assistant button integration with auto-discovery**
- **Execute shell commands via button presses**
- **Home Assistant switch integration with auto-discovery**
- **Execute shell commands with state management via switch toggles**
- **System monitoring with Home Assistant sensor discovery**
  - CPU load percentage (reported every 60 seconds)
  - CPU frequency (if available)
  - Memory usage: total RAM, free RAM in GB, and free percentage (reported every 30 seconds)

## Configuration

Edit `config.toml` to configure the daemon:

```toml
hostname = "my-device-01"          # Client identifier
mqtt_url = "your.mqtt.broker.com"  # MQTT broker URL
mqtt_port = 1883                   # MQTT broker port
username = "your_username"         # MQTT username
password = "your_password"         # MQTT password

topics = [                         # List of topics to subscribe to
    "sensors/temperature",
    "sensors/humidity",
    "alerts/motion"
]

update_interval_ms = 5000          # Reconnection interval (ms)

# Home Assistant Buttons (optional)
[[button]]
name = "Suspend"                   # Button name shown in Home Assistant
exec = "systemctl suspend"         # Shell command to execute on button press

[[button]]
name = "Reboot"
exec = "sudo reboot"

[[button]]
name = "Update System"
exec = "sudo apt update && sudo apt upgrade -y"

# Home Assistant Switches (optional)
[[switch]]
name = "Test Switch"               # Switch name shown in Home Assistant
exec = "echo Switch state:"        # Shell command to execute with "on" or "off" argument
```

## Building and Running

1. Build the daemon:
   ```bash
   cargo build --release
   ```

2. Run the daemon:
   ```bash
   cargo run
   ```

   Or run the compiled binary:
   ```bash
   ./target/release/mqtt-daemon
   ```

## Home Assistant Integration

The daemon automatically publishes Home Assistant discovery messages for configured buttons. When you start the daemon:

1. **Discovery**: The daemon publishes discovery messages to `homeassistant/button/{hostname}_{button_name}/config`
2. **Button Creation**: Home Assistant automatically creates button entities
3. **Button Press**: When pressed in Home Assistant, it sends "PRESS" to `homeassistant/button/{hostname}_{button_name}/set`
4. **Command Execution**: The daemon executes the configured shell command

### Button Topics

For a device with hostname `hp-steffen` and a button named `Suspend`:
- **Discovery topic**: `homeassistant/button/hp-steffen_suspend/config`
- **Command topic**: `homeassistant/button/hp-steffen_suspend/set`

The daemon will automatically handle the naming and topic generation.

### Switch Integration

The daemon also supports Home Assistant switches that can be turned ON/OFF. Switch integration follows this flow:

1. **Discovery**: The daemon publishes discovery messages to `homeassistant/switch/{hostname}_{switch_name}/config`
2. **Switch Creation**: Home Assistant automatically creates switch entities
3. **Switch Command**: When toggled in Home Assistant, it sends "ON" or "OFF" to `homeassistant/switch/{hostname}_{switch_name}/set`
4. **Command Execution**: The daemon executes the configured shell command with "on" or "off" as an argument
5. **State Publishing**: If the command succeeds, the current state is published to the state topic. If it fails, an empty payload is published.

#### Switch Topics

For a device with hostname `rust-daemon` and a switch named `Test Switch`:
- **Discovery topic**: `homeassistant/switch/rust-daemon_test_switch/config`
- **Command topic**: `homeassistant/switch/rust-daemon_test_switch/set`
- **State topic**: `homeassistant/switch/rust-daemon_test_switch/state`

### System Monitoring Sensors

The daemon automatically creates Home Assistant sensors for system monitoring:

#### CPU Monitoring
- **CPU Load**: Reports system load average (1-minute) as a percentage
  - Topic: `homeassistant/sensor/{hostname}/cpu_load/state`
  - Update interval: 60 seconds
  - Unit: %

- **CPU Frequency**: Reports current CPU frequency (if available)
  - Topic: `homeassistant/sensor/{hostname}/cpu_frequency/state`
  - Update interval: 60 seconds
  - Unit: MHz

#### Memory Monitoring
- **Memory Total**: Total system RAM
  - Topic: `homeassistant/sensor/{hostname}/memory_total/state`
  - Update interval: 30 seconds
  - Unit: GB

- **Memory Free**: Available system RAM
  - Topic: `homeassistant/sensor/{hostname}/memory_free/state`
  - Update interval: 30 seconds
  - Unit: GB

- **Memory Free %**: Free memory percentage
  - Topic: `homeassistant/sensor/{hostname}/memory_free_pct/state`
  - Update interval: 30 seconds
  - Unit: %

All sensors are automatically discovered by Home Assistant and include proper device associations.

## Running as a System Service

To run as a systemd service on Linux:

1. Copy the binary to `/usr/local/bin/`
2. Create a service file at `/etc/systemd/system/mqtt-daemon.service`
3. Enable and start the service

Example service file:
```ini
[Unit]
Description=MQTT Daemon
After=network.target

[Service]
Type=simple
User=mqtt-daemon
WorkingDirectory=/opt/mqtt-daemon
ExecStart=/usr/local/bin/mqtt-daemon
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

## Dependencies

- `rumqttc` - MQTT client library
- `tokio` - Async runtime
- `serde` - Serialization framework
- `toml` - TOML parser
- `serde_json` - JSON serialization for Home Assistant discovery
- `tracing` - Structured logging
- `tracing-subscriber` - Logging output formatting and filtering
