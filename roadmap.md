
High Priority:
- Add proper logging with tracing
    - Add log levels: Support different verbosity levels (debug, info, warn, error)
- Split code into modules: The single main.rs file should be split into multiple modules (config.rs, mqtt.rs, homeassistant.rs, commands.rs)
- Add unit tests
- Better Home Assistant Integration
    - More entity types: Support sensors, switches, binary sensors beyond buttons
    - Discovery cleanup: Remove stale discovery entries on shutdown
    - Entity icons and categories: Better Home Assistant UI integration
- State publishing: Publish device state back to Home Assistant
    - CPU and RAM usage via sysinfo

Medium Priority:
- Async command execution
- Package as rpm
- Deal with suspend/shutdown, via systemd
- API from system


Low Priority:
- Custom error types
- Configuration validation
- Credential management: Support for external secret management
- Advanced Home Assistant features
- Performance metrics: Track connection status, message counts, command execution success/failure rates
- Configuration hot-reload

On hold:
- Add TLS support (Needs work on broker side)

Out of scope:
- Multi-broker support: Connect to multiple MQTT brokers
- Plugin system: Allow extending functionality without code changes


1. Error Handling & Resilience
- Graceful shutdown: Handle SIGINT/SIGTERM for clean shutdown
- Circuit breaker pattern: For command execution failures
- Retry logic: Exponential backoff for MQTT reconnections
- Dead letter handling: Handle failed message processing

7. Performance & Resource Management
- Connection pooling: Reuse MQTT connections efficiently
- Message queuing: Handle message bursts without blocking
- Resource limits: Set limits on concurrent command executions
- Memory management: Monitor and limit memory usage

9. Command Execution
- Async command execution: Don't block MQTT loop on long-running commands
- Command timeouts: Prevent hanging on stuck commands
- Output capture: Optional command output publishing to MQTT
- Command templates: Support for parameterized commands
10. Documentation & Deployment
- API documentation: Generate docs with cargo doc
- Systemd service: Service file for Linux deployment
- Configuration examples: More comprehensive examples
- Migration guide: For upgrading between versions
11. Monitoring & Debugging
- Debug mode: Verbose output for troubleshooting
- Connection status: Publish daemon status to MQTT
- Command history: Log executed commands with results
- Performance metrics: Track message processing latency

