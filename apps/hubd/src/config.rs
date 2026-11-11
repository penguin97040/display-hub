//! `hubd.toml`: where to listen and keep data. It holds no secrets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 8443;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Address and port for HTTPS. IPv4 on all interfaces by default; the hub still refuses any
    /// source address outside the local network.
    pub listen: SocketAddr,
    /// Database and TLS identity. Readable by the service user only.
    pub data_dir: PathBuf,
    /// Accept link-local and unique local IPv6 sources (never global ones).
    pub allow_ipv6: bool,
    /// Poll interval suggested to displays, in seconds (30-3600).
    pub refresh_after_seconds: u32,
    /// Host names or addresses printed in pairing strings and given to displays, in the order to
    /// try. Empty means this machine's name with `.local` (Raspberry Pi OS answers to it by default).
    pub advertise_hosts: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            data_dir: PathBuf::from("/var/lib/display-hub"),
            allow_ipv6: false,
            refresh_after_seconds: 300,
            advertise_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("couldn't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path} isn't valid: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

impl Config {
    /// Reads `path`. A missing file is an error; an empty one means every default.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.into(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|e| ConfigError::Invalid {
            path: path.into(),
            detail: e.to_string(),
        })?;
        config.check().map_err(|detail| ConfigError::Invalid {
            path: path.into(),
            detail,
        })?;
        Ok(config)
    }

    pub fn check(&self) -> Result<(), String> {
        if !hub_protocol::limits::REFRESH_RANGE.contains(&self.refresh_after_seconds) {
            return Err("refresh_after_seconds must be 30-3600".into());
        }
        if self.listen.port() == 0 {
            return Err("listen needs a port".into());
        }
        if self.advertise_hosts.len() > hub_protocol::provisioning::MAX_HOSTS {
            return Err(format!(
                "at most {} advertise_hosts",
                hub_protocol::provisioning::MAX_HOSTS
            ));
        }
        if self
            .advertise_hosts
            .iter()
            .any(|h| h.is_empty() || h.contains(char::is_whitespace))
        {
            return Err("advertise_hosts must be plain host names or addresses".into());
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("hub.db")
    }

    pub fn identity_dir(&self) -> PathBuf {
        self.data_dir.join("identity")
    }

    /// Where displays and the manager should look for the hub.
    pub fn hosts(&self) -> Vec<String> {
        if !self.advertise_hosts.is_empty() {
            return self.advertise_hosts.clone();
        }
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
            .unwrap_or_else(|| "raspberrypi".into());
        vec![format!("{name}.local")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_defaults_and_mistakes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hubd.toml");
        std::fs::write(&p, "").unwrap();
        assert_eq!(Config::load(&p).unwrap(), Config::default());
        std::fs::write(
            &p,
            "listen = \"192.168.1.20:8443\"\nadvertise_hosts = [\"hub.local\"]\n",
        )
        .unwrap();
        assert_eq!(Config::load(&p).unwrap().listen.port(), 8443);
        for bad in [
            "listen = 5",
            "lisen = \"0.0.0.0:1\"",
            "refresh_after_seconds = 1",
            "[[[",
            "\u{0}",
        ] {
            std::fs::write(&p, bad).unwrap();
            assert!(
                matches!(Config::load(&p), Err(ConfigError::Invalid { .. })),
                "{bad}"
            );
        }
        assert!(matches!(
            Config::load(&dir.path().join("missing.toml")),
            Err(ConfigError::Read { .. })
        ));
    }
}
