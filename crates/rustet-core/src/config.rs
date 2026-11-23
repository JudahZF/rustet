//! The engine's YAML config file, `config.yaml` in the engine data directory.
//!
//! The shell writes the file and patches `drm-base-dir` on every start. Older files carry many
//! more keys (download folders, lyrics and cover options, ports) left from the downloader the
//! engine grew out of. Nothing reads them any more; they are accepted and ignored.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::appdir;

/// The engine settings read from `config.yaml`. A missing key takes its default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// The directory that holds `libdrm_client.so` and `files/`. `None` means auto-discover
    /// `drm/` under the working directory.
    #[serde(deserialize_with = "non_empty_path")]
    pub drm_binary_path: Option<PathBuf>,
    /// The directory of the DRM session (`mpl_db` parent). `None` means `files/` under the
    /// DRM binary directory.
    #[serde(deserialize_with = "non_empty_path")]
    pub drm_base_dir: Option<PathBuf>,
    /// The minimum export (download) rate in KiB/s while playback streams. `None` (0 or less
    /// in the file) means the engine default.
    #[serde(deserialize_with = "positive_kbps")]
    pub export_throttle_floor_kbps: Option<u32>,
}

/// Why the config file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<serde_saphyr::Error>,
    },
}

impl Config {
    /// Where the shell keeps the file: `<config dir>/engine-data/config.yaml`.
    pub fn default_path() -> PathBuf {
        appdir::config_dir().join("engine-data").join("config.yaml")
    }

    /// Reads and parses the file at `path`. An empty file is all defaults. The engine logs the
    /// error and runs with [`Config::default`] when this fails.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    fn parse(text: &str) -> Result<Self, serde_saphyr::Error> {
        // A document with no content (empty, or only comments) is the zero config, as in the
        // Go engine.
        Ok(serde_saphyr::from_str::<Option<Self>>(text)?.unwrap_or_default())
    }
}

/// `""` and `null` both mean "not set".
fn non_empty_path<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

fn positive_kbps<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<i64>::deserialize(d)?
        .filter(|&v| v > 0)
        .map(|v| u32::try_from(v).unwrap_or(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stub the shell writes on first start, plus keys older builds left behind.
    #[test]
    fn loads_the_shell_stub_and_ignores_legacy_keys() {
        let text = r#"storefront: us
media-user-token: ""
authorization-token: ""
decrypt-m3u8-port: 127.0.0.1:10020
get-m3u8-port: 127.0.0.1:20020
get-m3u8-from-device: true
get-m3u8-mode: hires
aac-type: aac-lc
alac-max: 192000
drm-base-dir: "/home/u/.config/rustet/drm/files"
lrc-format: lrc
cover-size: 5000x5000
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(
            cfg.drm_base_dir.as_deref(),
            Some(Path::new("/home/u/.config/rustet/drm/files"))
        );
        assert_eq!(cfg.drm_binary_path, None);
        assert_eq!(cfg.export_throttle_floor_kbps, None);
    }

    #[test]
    fn reads_every_engine_key() {
        let text = "drm-binary-path: /opt/rustet/drm\ndrm-base-dir: /var/drm\nexport-throttle-floor-kbps: 256\n";
        let cfg = Config::parse(text).unwrap();
        assert_eq!(
            cfg,
            Config {
                drm_binary_path: Some("/opt/rustet/drm".into()),
                drm_base_dir: Some("/var/drm".into()),
                export_throttle_floor_kbps: Some(256),
            }
        );
    }

    /// The shell writes paths with JSON string escaping, which is valid YAML.
    #[test]
    fn json_escaped_windows_path() {
        let cfg = Config::parse(
            r#"drm-base-dir: "C:\\Users\\Zoë\\AppData\\Roaming\\rustet\\drm\\files""#,
        )
        .unwrap();
        assert_eq!(
            cfg.drm_base_dir.as_deref(),
            Some(Path::new(r"C:\Users\Zoë\AppData\Roaming\rustet\drm\files"))
        );
    }

    #[test]
    fn empty_and_null_values_are_unset() {
        let cfg =
            Config::parse("drm-binary-path: \"\"\ndrm-base-dir:\nexport-throttle-floor-kbps: 0\n")
                .unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(
            Config::parse("export-throttle-floor-kbps: -5").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn empty_document_is_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::parse("# only a comment\n").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load(&dir.path().join("config.yaml")).unwrap_err();
        assert!(
            matches!(err, ConfigError::Read { ref source, .. } if source.kind() == io::ErrorKind::NotFound)
        );
    }
}
