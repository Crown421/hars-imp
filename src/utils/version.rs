/// Version and package information extracted from Cargo.toml at compile time
pub struct VersionInfo {
    pub version: String,
    pub name: String,
    pub repository: String,
}

impl Default for VersionInfo {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            name: env!("CARGO_PKG_NAME").to_string(),
            repository: env!("CARGO_PKG_REPOSITORY").to_string(),
        }
    }
}

impl VersionInfo {
    /// Get a static instance of version information
    pub fn get() -> &'static VersionInfo {
        static VERSION_INFO: std::sync::OnceLock<VersionInfo> = std::sync::OnceLock::new();
        VERSION_INFO.get_or_init(VersionInfo::default)
    }
}
