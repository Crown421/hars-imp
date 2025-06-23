// Main dbus module - exports public API

mod inhibitor;
mod notifications;
mod power_management;
pub mod status;

// Re-export public types and functions
pub use inhibitor::PowerManager;
pub use notifications::send_desktop_notification;
pub use power_management::{
    PowerEvent, PowerEventHandler, handle_power_events, setup_power_monitoring,
};
pub use status::{StatusManager, create_status_component};
