// Main dbus module - exports public API

mod inhibitor;
mod power_events;

// Re-export public types and functions
pub use inhibitor::{PowerManager, SuspendInhibitor};
pub use power_events::{
    handle_power_events, handle_resume_actions, handle_suspend_actions, setup_power_monitoring,
    PowerEvent,
};
