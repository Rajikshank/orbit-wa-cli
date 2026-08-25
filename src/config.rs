//! Configuration and path resolution.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Every mutable Orbit file is rooted here, making backup and removal explicit.
#[derive(Clone, Debug)]
pub struct OrbitPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub database: PathBuf,
    pub logs: PathBuf,
    pub bin: PathBuf,
    pub whatsapp_store: PathBuf,
}

impl OrbitPaths {
    /// Resolve the default local-first `~/.orbit` directory.
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().context("could not determine the current user's home")?;
        Ok(Self::for_root(home.join(".orbit")))
    }

    /// Build paths below an explicit root; tests use this to avoid user data.
    #[must_use]
    pub fn for_root(root: PathBuf) -> Self {
        Self {
            config: root.join("config.toml"),
            database: root.join("orbit.db"),
            logs: root.join("logs"),
            bin: root.join("bin"),
            whatsapp_store: root.join("connectors").join("whatsapp"),
            root,
        }
    }

    pub fn create(&self) -> Result<()> {
        for dir in [&self.root, &self.logs, &self.bin, &self.whatsapp_store] {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            #[cfg(unix)]
            secure_directory(dir)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn wacli_binary(&self) -> PathBuf {
        self.bin
            .join(if cfg!(windows) { "wacli.exe" } else { "wacli" })
    }

    #[must_use]
    pub fn ipc_name(&self) -> String {
        // One endpoint per root permits isolated test and multi-profile daemons.
        let digest = sha256_short(self.root.to_string_lossy().as_bytes());
        if cfg!(windows) {
            format!(r"\\.\pipe\orbit-{digest}")
        } else {
            self.root.join("orbit.sock").to_string_lossy().into_owned()
        }
    }
}

fn sha256_short(input: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input))[..16].to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Explicit override; when absent Orbit uses its checksum-verified bundled driver.
    pub wacli_path: Option<PathBuf>,
    pub webhook_port: u16,
    pub reconcile_interval_seconds: u64,
    pub reconcile_limit: u32,
    pub max_messages: u64,
    pub max_database_size: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wacli_path: None,
            webhook_port: 38_217,
            reconcile_interval_seconds: 60,
            reconcile_limit: 1_000,
            max_messages: 250_000,
            max_database_size: "2GB".to_owned(),
        }
    }
}

impl Config {
    pub fn load(paths: &OrbitPaths) -> Result<Self> {
        if !paths.config.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&paths.config)
            .with_context(|| format!("read {}", paths.config.display()))?;
        toml::from_str(&raw).context("parse Orbit config")
    }

    pub fn save(&self, paths: &OrbitPaths) -> Result<()> {
        paths.create()?;
        let encoded = toml::to_string_pretty(self).context("encode Orbit config")?;
        fs::write(&paths.config, encoded)
            .with_context(|| format!("write {}", paths.config.display()))?;
        #[cfg(unix)]
        secure_file(&paths.config)?;
        Ok(())
    }

    #[must_use]
    pub fn resolved_wacli(&self, paths: &OrbitPaths) -> PathBuf {
        self.wacli_path
            .clone()
            .unwrap_or_else(|| paths.wacli_binary())
    }
}

#[cfg(unix)]
fn secure_directory(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure {}", path.display()))
}

#[cfg(unix)]
fn secure_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_preserves_operational_limits() {
        let temp = tempfile::tempdir().unwrap();
        let paths = OrbitPaths::for_root(temp.path().to_path_buf());
        let expected = Config {
            reconcile_interval_seconds: 17,
            reconcile_limit: 432,
            ..Config::default()
        };
        expected.save(&paths).unwrap();
        let actual = Config::load(&paths).unwrap();
        assert_eq!(actual.reconcile_interval_seconds, 17);
        assert_eq!(actual.reconcile_limit, 432);
    }

    #[test]
    fn ipc_name_is_stable_and_root_scoped() {
        let a = OrbitPaths::for_root(PathBuf::from("one"));
        let b = OrbitPaths::for_root(PathBuf::from("two"));
        assert_eq!(a.ipc_name(), a.ipc_name());
        assert_ne!(a.ipc_name(), b.ipc_name());
    }

    #[cfg(unix)]
    #[test]
    fn state_directories_and_config_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let paths = OrbitPaths::for_root(temp.path().join("orbit"));
        paths.create().unwrap();
        Config::default().save(&paths).unwrap();
        assert_eq!(
            fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.config).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
