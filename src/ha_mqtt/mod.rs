pub mod discovery;
pub mod handlers;
pub mod init;

// Re-export all public items to maintain compatibility
pub use discovery::{
    create_shared_device, create_shared_origin, publish_discovery, publish_unified_discovery,
    ComponentType, DeviceDiscoveryBuilder, HomeAssistantComponent, HomeAssistantDevice,
    HomeAssistantDeviceDiscovery, HomeAssistantOrigin,
};
pub use handlers::{TopicHandler, TopicHandlers};
pub use init::initialize_mqtt_connection;
