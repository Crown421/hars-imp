use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::config::AmbientLightMonitorConfig;
use crate::mqtt::discovery::{ComponentType, HomeAssistantComponent};

const IIO_AMBIENT_LIGHT_ROOT: &str = "/sys/bus/iio/devices";
const BACKLIGHT_AMBIENT_LIGHT_ROOT: &str = "/sys/class/backlight";
const IIO_AMBIENT_LIGHT_INPUT_FILENAME: &str = "in_illuminance_input";
const IIO_AMBIENT_LIGHT_RAW_FILENAME: &str = "in_illuminance_raw";
const IIO_AMBIENT_LIGHT_SCALE_FILENAME: &str = "in_illuminance_scale";
const IIO_AMBIENT_LIGHT_OFFSET_FILENAME: &str = "in_illuminance_offset";
const BACKLIGHT_AMBIENT_LIGHT_FILENAME: &str = "ambient_light_level";
pub(super) const AMBIENT_LIGHT_METRIC_NAME: &str = "Ambient Light";
pub(super) const AMBIENT_LIGHT_METRIC_KEY: &str = "ambient_light";

#[derive(Debug, Clone, PartialEq)]
pub(super) struct AmbientLightMonitor {
    source: AmbientLightSource,
}

#[derive(Debug, Clone, PartialEq)]
enum AmbientLightSource {
    IioInput {
        path: PathBuf,
    },
    IioRaw {
        raw_path: PathBuf,
        conversion: RawConversion,
    },
    Backlight {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum RawConversion {
    Lux { scale: f32, offset: f32 },
    RawFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplicitAmbientLightPathKind {
    IioInput,
    IioRaw,
    Backlight,
}

impl AmbientLightMonitor {
    pub(super) fn from_config(config: &AmbientLightMonitorConfig) -> Option<Self> {
        Self::from_config_with_roots(
            config,
            Path::new(IIO_AMBIENT_LIGHT_ROOT),
            Path::new(BACKLIGHT_AMBIENT_LIGHT_ROOT),
        )
    }

    pub(super) fn from_config_with_roots(
        config: &AmbientLightMonitorConfig,
        iio_root: &Path,
        backlight_root: &Path,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        if let Some(path) = &config.path {
            match Self::explicit(path) {
                Ok(monitor) => {
                    info!(
                        "Ambient light monitoring enabled with configured source '{}' ({})",
                        monitor.path().display(),
                        monitor.mode_description()
                    );
                    Some(monitor)
                }
                Err(err) => {
                    warn!(
                        "Ambient light monitoring disabled for this run: failed to use configured path '{}': {err}",
                        path
                    );
                    None
                }
            }
        } else {
            let monitor = Self::discover(iio_root, backlight_root);
            if let Some(monitor) = &monitor {
                info!(
                    "Ambient light monitoring enabled with discovered source '{}' ({})",
                    monitor.path().display(),
                    monitor.mode_description()
                );
            } else {
                info!("Ambient light monitoring enabled but no supported sensor was found");
            }
            monitor
        }
    }

    pub(super) fn explicit(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let Some(kind) = classify_explicit_ambient_light_path(&path) else {
            return Err(format!(
                "unsupported ambient light path '{}'",
                path.display()
            ));
        };

        let monitor = match kind {
            ExplicitAmbientLightPathKind::IioInput => Self {
                source: AmbientLightSource::IioInput { path },
            },
            ExplicitAmbientLightPathKind::IioRaw => Self::from_iio_raw_path(path),
            ExplicitAmbientLightPathKind::Backlight => Self {
                source: AmbientLightSource::Backlight { path },
            },
        };
        monitor.read_value()?;
        Ok(monitor)
    }

    fn discover(iio_root: &Path, backlight_root: &Path) -> Option<Self> {
        discover_iio_input(iio_root)
            .or_else(|| discover_iio_raw(iio_root))
            .or_else(|| discover_backlight(backlight_root))
    }

    fn from_iio_raw_path(raw_path: PathBuf) -> Self {
        Self {
            source: AmbientLightSource::IioRaw {
                conversion: classify_iio_raw_conversion(&raw_path),
                raw_path,
            },
        }
    }

    pub(super) fn discovery_component(
        &self,
        hostname: &str,
        state_topic: &str,
    ) -> (String, HomeAssistantComponent) {
        (
            format!("{hostname}_{AMBIENT_LIGHT_METRIC_KEY}"),
            HomeAssistantComponent {
                name: AMBIENT_LIGHT_METRIC_NAME.to_string(),
                unique_id: format!("{hostname}_{AMBIENT_LIGHT_METRIC_KEY}"),
                component_type: ComponentType::Sensor {
                    state_topic: state_topic.to_string(),
                    device_class: None,
                    unit_of_measurement: self.unit_of_measurement().map(str::to_string),
                    value_template: Some(format!(
                        "{{{{ value_json.{AMBIENT_LIGHT_METRIC_KEY} }}}}"
                    )),
                    icon: None,
                },
            },
        )
    }

    pub(super) fn read_value(&self) -> Result<f32, String> {
        match &self.source {
            AmbientLightSource::IioInput { path } | AmbientLightSource::Backlight { path } => {
                read_published_numeric_file(path)
            }
            AmbientLightSource::IioRaw {
                raw_path,
                conversion,
            } => {
                let raw = parse_numeric_file(raw_path)?;
                match conversion {
                    // For raw sensors we decide the unit contract and metadata
                    // at startup so subsequent samples only need the raw value.
                    RawConversion::Lux { scale, offset } => {
                        Ok(round_to_2dp((raw + offset) * scale))
                    }
                    RawConversion::RawFallback => Ok(round_to_2dp(raw)),
                }
            }
        }
    }

    fn unit_of_measurement(&self) -> Option<&'static str> {
        match &self.source {
            AmbientLightSource::IioInput { .. } => Some("lx"),
            AmbientLightSource::IioRaw { conversion, .. } => match conversion {
                RawConversion::Lux { .. } => Some("lx"),
                RawConversion::RawFallback => None,
            },
            AmbientLightSource::Backlight { .. } => None,
        }
    }

    fn path(&self) -> &Path {
        match &self.source {
            AmbientLightSource::IioInput { path } | AmbientLightSource::Backlight { path } => path,
            AmbientLightSource::IioRaw { raw_path, .. } => raw_path,
        }
    }

    fn mode_description(&self) -> &'static str {
        match &self.source {
            AmbientLightSource::IioInput { .. } => "iio input lux",
            AmbientLightSource::IioRaw {
                conversion: RawConversion::Lux { .. },
                ..
            } => "iio raw lux conversion",
            AmbientLightSource::IioRaw {
                conversion: RawConversion::RawFallback,
                ..
            } => "iio raw numeric fallback",
            AmbientLightSource::Backlight { .. } => "backlight ambient level",
        }
    }
}

fn classify_explicit_ambient_light_path(path: &Path) -> Option<ExplicitAmbientLightPathKind> {
    match path.file_name()?.to_str()? {
        IIO_AMBIENT_LIGHT_INPUT_FILENAME => Some(ExplicitAmbientLightPathKind::IioInput),
        IIO_AMBIENT_LIGHT_RAW_FILENAME => Some(ExplicitAmbientLightPathKind::IioRaw),
        BACKLIGHT_AMBIENT_LIGHT_FILENAME => Some(ExplicitAmbientLightPathKind::Backlight),
        _ => None,
    }
}

fn classify_iio_raw_conversion(raw_path: &Path) -> RawConversion {
    let Some(parent) = raw_path.parent() else {
        return RawConversion::RawFallback;
    };

    let scale_path = parent.join(IIO_AMBIENT_LIGHT_SCALE_FILENAME);
    let Ok(scale) = parse_numeric_file(&scale_path) else {
        return RawConversion::RawFallback;
    };

    let offset_path = parent.join(IIO_AMBIENT_LIGHT_OFFSET_FILENAME);
    let offset = if offset_path.exists() {
        match parse_numeric_file(&offset_path) {
            Ok(offset) => offset,
            Err(_) => {
                return RawConversion::RawFallback;
            }
        }
    } else {
        0.0
    };

    RawConversion::Lux { scale, offset }
}

fn discover_iio_input(iio_root: &Path) -> Option<AmbientLightMonitor> {
    discover_candidate_paths(iio_root, IIO_AMBIENT_LIGHT_INPUT_FILENAME)
        .into_iter()
        .find_map(|path| {
            let monitor = AmbientLightMonitor {
                source: AmbientLightSource::IioInput { path },
            };
            validate_candidate(monitor)
        })
}

fn discover_iio_raw(iio_root: &Path) -> Option<AmbientLightMonitor> {
    discover_candidate_paths(iio_root, IIO_AMBIENT_LIGHT_RAW_FILENAME)
        .into_iter()
        .find_map(|raw_path| validate_candidate(AmbientLightMonitor::from_iio_raw_path(raw_path)))
}

fn discover_backlight(backlight_root: &Path) -> Option<AmbientLightMonitor> {
    discover_candidate_paths(backlight_root, BACKLIGHT_AMBIENT_LIGHT_FILENAME)
        .into_iter()
        .find_map(|path| {
            let monitor = AmbientLightMonitor {
                source: AmbientLightSource::Backlight { path },
            };
            validate_candidate(monitor)
        })
}

fn validate_candidate(monitor: AmbientLightMonitor) -> Option<AmbientLightMonitor> {
    match monitor.read_value() {
        Ok(_) => Some(monitor),
        Err(err) => {
            debug!(
                "Skipping ambient light candidate '{}': {err}",
                monitor.path().display()
            );
            None
        }
    }
}

fn discover_candidate_paths(root: &Path, filename: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut candidates: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(filename))
        .filter(|candidate| candidate.exists())
        .collect();
    candidates.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    candidates
}

fn parse_numeric_file(path: &Path) -> Result<f32, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "ambient light source '{}' was empty",
            path.display()
        ));
    }

    let value = trimmed
        .parse::<f32>()
        .map_err(|err| format!("failed to parse '{}': {err}", path.display()))?;
    Ok(value)
}

fn read_published_numeric_file(path: &Path) -> Result<f32, String> {
    parse_numeric_file(path).map(round_to_2dp)
}

fn round_to_2dp(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ambient_light_config(
        enabled: bool,
        path: Option<impl Into<String>>,
    ) -> AmbientLightMonitorConfig {
        AmbientLightMonitorConfig {
            enabled,
            path: path.map(Into::into),
        }
    }

    fn write_sensor_file(root: &Path, device_dir: &str, filename: &str, value: &str) -> PathBuf {
        let path = root.join(device_dir).join(filename);
        std::fs::create_dir_all(path.parent().expect("ambient light parent")).unwrap();
        std::fs::write(&path, value).unwrap();
        path
    }

    #[test]
    fn configured_iio_input_claims_lux() {
        let temp = tempdir().unwrap();
        let path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_INPUT_FILENAME,
            "123.45\n",
        );
        let monitor = AmbientLightMonitor::explicit(path).expect("ambient light source");

        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
        assert_eq!(monitor.read_value().unwrap(), 123.45);
    }

    #[test]
    fn configured_iio_raw_with_scale_and_offset_publishes_converted_lux() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "100\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.5\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_OFFSET_FILENAME,
            "2\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
        assert_eq!(monitor.read_value().unwrap(), 51.0);
    }

    #[test]
    fn configured_iio_raw_preserves_small_scale_precision() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "1000\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.004\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
        assert_eq!(monitor.read_value().unwrap(), 4.0);
    }

    #[test]
    fn configured_iio_raw_without_offset_uses_zero_offset() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "100\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.25\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
        assert_eq!(monitor.read_value().unwrap(), 25.0);
    }

    #[test]
    fn configured_iio_raw_without_scale_falls_back_to_raw_numeric() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "42\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), None);
        assert_eq!(monitor.read_value().unwrap(), 42.0);
    }

    #[test]
    fn configured_iio_raw_with_invalid_scale_falls_back_to_raw_numeric() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "42\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "not-a-number\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), None);
        assert_eq!(monitor.read_value().unwrap(), 42.0);
    }

    #[test]
    fn configured_iio_raw_with_invalid_offset_falls_back_to_raw_numeric() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "42\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.5\n",
        );
        write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_OFFSET_FILENAME,
            "broken\n",
        );

        let monitor = AmbientLightMonitor::explicit(raw_path).expect("ambient light source");
        assert_eq!(monitor.unit_of_measurement(), None);
        assert_eq!(monitor.read_value().unwrap(), 42.0);
    }

    #[test]
    fn invalid_configured_raw_path_disables_sensor_without_fallback() {
        let temp = tempdir().unwrap();
        let explicit_path = temp
            .path()
            .join("iio:device0")
            .join(IIO_AMBIENT_LIGHT_RAW_FILENAME);
        let discovery_root = temp.path().join("backlight");
        write_sensor_file(
            &discovery_root,
            "intel_backlight",
            BACKLIGHT_AMBIENT_LIGHT_FILENAME,
            "88\n",
        );

        let source = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Some(explicit_path.display().to_string())),
            temp.path(),
            &discovery_root,
        );

        assert!(source.is_none());
    }

    #[test]
    fn auto_discovery_prefers_iio_input_over_iio_raw() {
        let temp = tempdir().unwrap();
        let iio_root = temp.path().join("iio");
        let backlight_root = temp.path().join("backlight");
        let input_path = write_sensor_file(
            &iio_root,
            "iio:device0",
            IIO_AMBIENT_LIGHT_INPUT_FILENAME,
            "17\n",
        );
        write_sensor_file(
            &iio_root,
            "iio:device1",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "50\n",
        );
        write_sensor_file(
            &iio_root,
            "iio:device1",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.5\n",
        );
        write_sensor_file(
            &backlight_root,
            "intel_backlight",
            BACKLIGHT_AMBIENT_LIGHT_FILENAME,
            "88\n",
        );

        let monitor = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Option::<String>::None),
            &iio_root,
            &backlight_root,
        )
        .expect("ambient light source");

        assert_eq!(monitor.path(), input_path.as_path());
        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
    }

    #[test]
    fn auto_discovery_selects_iio_raw_when_no_input_exists() {
        let temp = tempdir().unwrap();
        let iio_root = temp.path().join("iio");
        let raw_path = write_sensor_file(
            &iio_root,
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "60\n",
        );
        write_sensor_file(
            &iio_root,
            "iio:device0",
            IIO_AMBIENT_LIGHT_SCALE_FILENAME,
            "0.5\n",
        );

        let monitor = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Option::<String>::None),
            &iio_root,
            temp.path(),
        )
        .expect("ambient light source");

        assert_eq!(monitor.path(), raw_path.as_path());
        assert_eq!(monitor.unit_of_measurement(), Some("lx"));
        assert_eq!(monitor.read_value().unwrap(), 30.0);
    }

    #[test]
    fn auto_discovery_falls_back_to_backlight_when_no_iio_source_exists() {
        let temp = tempdir().unwrap();
        let backlight_root = temp.path().join("backlight");
        let backlight_path = write_sensor_file(
            &backlight_root,
            "intel_backlight",
            BACKLIGHT_AMBIENT_LIGHT_FILENAME,
            "88\n",
        );

        let monitor = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Option::<String>::None),
            temp.path(),
            &backlight_root,
        )
        .expect("ambient light source");

        assert_eq!(monitor.path(), backlight_path.as_path());
        assert_eq!(monitor.unit_of_measurement(), None);
    }

    #[test]
    fn auto_discovery_keeps_raw_selection_deterministic() {
        let temp = tempdir().unwrap();
        let iio_root = temp.path().join("iio");
        let first_path = write_sensor_file(
            &iio_root,
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "15\n",
        );
        write_sensor_file(
            &iio_root,
            "iio:device1",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "25\n",
        );

        let monitor = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Option::<String>::None),
            &iio_root,
            temp.path(),
        )
        .expect("ambient light source");

        assert_eq!(monitor.path(), first_path.as_path());
    }

    #[test]
    fn raw_path_is_accepted_by_config_detection() {
        let temp = tempdir().unwrap();
        let raw_path = write_sensor_file(
            temp.path(),
            "iio:device0",
            IIO_AMBIENT_LIGHT_RAW_FILENAME,
            "77\n",
        );
        let monitor = AmbientLightMonitor::from_config_with_roots(
            &ambient_light_config(true, Some(raw_path.display().to_string())),
            temp.path(),
            temp.path(),
        )
        .expect("ambient light source");

        assert_eq!(monitor.path(), raw_path.as_path());
    }
}
