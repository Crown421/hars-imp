// Main dbus module - exports public API

mod inhibitor;
mod power_management;
pub mod status;

// Re-export public types and functions
pub use inhibitor::PowerManager;
pub use power_management::{
    handle_power_events, setup_power_monitoring, PowerEvent, PowerEventHandler,
};
pub use status::{create_status_component, StatusManager};
