//! XDG-located persistent config for API keys.
//!
//! Path: `$XDG_CONFIG_HOME/heightmap2brz/config.toml`
//! (typically `~/.config/heightmap2brz/config.toml`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CONFIG_DIR_NAME: &str = "heightmap2brz";
const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) opentopo_api_key: Option<String>,
    #[serde(default)]
    pub(crate) mapbox_token: Option<String>,
}

impl Config {
    pub(crate) fn path() -> Result<PathBuf, ConfigError> {
        let base = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(base.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
    }

    pub(crate) fn load() -> Result<Self, ConfigError> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path).map_err(ConfigError::Io)?;
        let parsed: Self = toml::from_str(&raw).map_err(ConfigError::TomlParse)?;
        Ok(parsed)
    }

    /// Persist this config to disk at `Self::path()`.
    ///
    /// # Postconditions
    /// - The file exists at `Self::path()` after the call returns `Ok(())`
    /// - On Unix, file mode is `0o600` **from creation** (no world-readable
    ///   window), via a 0600 temp file + atomic rename — see `write_private`.
    pub(crate) fn save(&self) -> Result<(), ConfigError> {
        let path = Self::path()?;
        let parent = path.parent().ok_or(ConfigError::NoConfigDir)?;
        std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        self.save_to(&path)
    }

    /// Serialize and write to an explicit path (testable without XDG env).
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let serialized = toml::to_string_pretty(self).map_err(ConfigError::TomlSerialize)?;
        write_private(path, serialized.as_bytes())?;
        debug_assert!(
            path.exists(),
            "Config::save_to postcondition: file must exist at {path:?} after successful write",
        );
        Ok(())
    }

    pub(crate) fn opentopo_key_set(&self) -> bool {
        self.opentopo_api_key
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    pub(crate) fn mapbox_token_set(&self) -> bool {
        self.mapbox_token
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

/// Write `bytes` to `path` so the file is mode 0o600 from the instant it
/// exists on disk: write to a unique per-process temp opened with
/// `create_new` + mode 0o600, fsync, then atomically rename over the
/// destination. `create_new` (not `create`) is essential — `mode()` is only
/// honored when the file is actually *created*, so reusing a pre-existing or
/// stale temp would silently keep its old (possibly world-readable) mode and
/// the rename would publish a 0644 secrets file. The temp is cleared first and
/// removed on any pre-rename error so no orphan (and thus no stale temp for a
/// later save to inherit) is left behind.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    // Clear a stale same-pid temp (race-free: the pid is exclusively ours) so
    // the create_new below always creates a fresh 0o600 file.
    let _ = std::fs::remove_file(&tmp);
    let result = write_new_0600_then_rename(&tmp, path, bytes);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn write_new_0600_then_rename(tmp: &Path, dest: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
        .map_err(ConfigError::Io)?;
    f.write_all(bytes).map_err(ConfigError::Io)?;
    f.sync_all().map_err(ConfigError::Io)?;
    std::fs::rename(tmp, dest).map_err(ConfigError::Io)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    // Windows ACL hardening is out of scope for the current target list.
    std::fs::write(path, bytes).map_err(ConfigError::Io)
}

#[derive(Debug)]
pub(crate) enum ConfigError {
    NoConfigDir,
    Io(std::io::Error),
    TomlParse(toml::de::Error),
    TomlSerialize(toml::ser::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigDir => write!(f, "no XDG config directory available"),
            Self::Io(e) => write!(f, "config IO error: {e}"),
            // .message() only — toml's full Display echoes the offending line
            // verbatim, and this file holds API tokens that must not reach the
            // GUI or a log line.
            Self::TomlParse(e) => {
                write!(f, "config parse error: {} (file contents not shown — fix or delete the config file)", e.message())
            }
            Self::TomlSerialize(e) => write!(f, "config serialize error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoConfigDir => None,
            Self::Io(e) => Some(e),
            Self::TomlParse(e) => Some(e),
            Self::TomlSerialize(e) => Some(e),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn save_to_writes_mode_0600_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("h2brz-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");

        let cfg = Config {
            opentopo_api_key: Some("secret-key".to_owned()),
            mapbox_token: None,
        };
        cfg.save_to(&path).expect("save_to should succeed");

        // The externally-observable security postcondition: mode is exactly 0600.
        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must be written 0600, got {mode:o}");

        // And the content actually persisted (the key round-trips through load-by-path).
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("opentopo_api_key"), "saved config must contain the key");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_to_hardens_even_with_stale_world_readable_temp() {
        // Regression guard for the create()-vs-create_new() bug: pre-plant a
        // world-readable temp at the exact name write_private uses, then assert
        // the FINAL config is still 0600. With the old create(true) code this
        // failed (the 0644 temp was reused and renamed into place).
        let dir = std::env::temp_dir().join(format!("h2brz-cfg-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        let stale_tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&stale_tmp, b"stale 0644 leftover").expect("plant stale temp");
        std::fs::set_permissions(&stale_tmp, std::fs::Permissions::from_mode(0o644))
            .expect("chmod stale temp 0644");

        let cfg = Config { opentopo_api_key: Some("k".to_owned()), mapbox_token: None };
        cfg.save_to(&path).expect("save over a stale temp");

        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "final config must be 0600 despite a stale 0644 temp, got {mode:o}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
