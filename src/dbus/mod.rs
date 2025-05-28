// Main dbus module - exports public API

mod inhibitor;
mod power_management;

// Re-export public types and functions
pub use inhibitor::PowerManager;
pub use power_management::{
    handle_power_events, setup_power_monitoring, PowerEvent, PowerEventHandler,
};
