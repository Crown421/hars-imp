# MQTT Daemon

A minimal and efficient Rust daemon for listening to MQTT topics and processing messages.

## Features

- Reads configuration from a TOML file
- Connects to MQTT broker with authentication
- Subscribes to multiple topics
- Prints received messages with timestamps
- Automatic reconnection on connection failures
- **Home Assistant button integration with auto-discovery**
- **Execute shell commands via button presses**

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
- `chrono` - Date and time handling
- `serde_json` - JSON serialization for Home Assistant discovery
