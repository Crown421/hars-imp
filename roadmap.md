Top Priority:
- handle shutdown
- Scrub config (with password....)
- Upload to GH
- Build rpm (via cargo-rpm)
- Move matcha to switch (needs proper "switch" object, and possibly a new script)
- Also add light/ dark mode switch?



High Priority:
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

