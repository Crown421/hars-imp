// Main dbus module - exports public API

mod inhibitor;
mod notifications;
mod power_management;
pub mod status;

// Re-export public types and functions
pub use inhibitor::PowerManager;
pub use notifications::{send_desktop_notification, test_notification_service};
pub use power_management::{
    handle_power_events, setup_power_monitoring, PowerEvent, PowerEventHandler,
};
pub use status::{create_status_component, StatusManager};
