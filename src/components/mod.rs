// components module - Contains component implementations for different MQTT entity types

pub mod buttons;
pub mod switch;
pub mod system_sensors;

// Re-export commonly used items for convenience
pub use buttons::create_button_components_and_setup;
pub use switch::create_switch_components_and_setup;
pub use system_sensors::{create_system_sensor_components, SystemMonitor};
