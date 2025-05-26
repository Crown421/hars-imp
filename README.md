# MQTT Daemon

A minimal and efficient Rust daemon for listening to MQTT topics and processing messages.

## Features

- Reads configuration from a TOML file
- Connects to MQTT broker with authentication
- Subscribes to multiple topics
- Prints received messages with timestamps
- Automatic reconnection on connection failures

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
