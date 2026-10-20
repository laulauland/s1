//! `~/.config/s1/config.toml`: listeners and log level. Later stages add
//! queue and cache sizes. Unknown keys are ignored.

use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;

/// Default TCP listener.
pub const DEFAULT_TCP: &str = "127.0.0.1:7341";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: Listen,
    pub log_level: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Listen {
    /// TCP address; must be a loopback address.
    pub tcp: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: Listen::default(),
            log_level: "info".into(),
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            tcp: DEFAULT_TCP.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
}

impl Config {
    /// Read the file; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t).map_err(|message| ConfigError::Parse {
                path: path.display().to_string(),
                message,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        c.tcp_addr()?;
        Ok(c)
    }

    /// The TCP address. Only loopback addresses are allowed.
    pub fn tcp_addr(&self) -> Result<SocketAddr, String> {
        let addr: SocketAddr = self
            .listen
            .tcp
            .parse()
            .map_err(|e| format!("listen.tcp `{}`: {e}", self.listen.tcp))?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "listen.tcp `{addr}` is not a loopback address; s1 listens on 127.0.0.1 only"
            ));
        }
        Ok(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.tcp_addr().unwrap().to_string(), DEFAULT_TCP);
        assert_eq!(c.log_level, "info");
    }

    #[test]
    fn reads_listener_and_rejects_public_address() {
        let c = Config::parse("[listen]\ntcp = \"127.0.0.1:0\"").unwrap();
        assert_eq!(c.tcp_addr().unwrap().port(), 0);
        assert!(Config::parse("[listen]\ntcp = \"0.0.0.0:7341\"").is_err());
    }
}
