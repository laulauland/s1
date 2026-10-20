//! File locations. Every path s1 uses comes from here.
//!
//! | Path | Default | Override |
//! | --- | --- | --- |
//! | auth file | `~/.s1/auth.json` | `S1_HOME` |
//! | config dir | `~/.config/s1` | `XDG_CONFIG_HOME` |
//! | state dir (logs, pid) | `~/.local/state/s1` | `XDG_STATE_HOME` |
//! | cache dir | `~/.cache/s1` | `XDG_CACHE_HOME` |
//! | socket | `$XDG_RUNTIME_DIR/s1.sock` | `S1_SOCKET`; falls back to the state dir |

use std::path::PathBuf;

/// Resolved file locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub s1_home: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub socket: PathBuf,
}

impl Paths {
    /// Resolve from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Resolve with a custom variable lookup (tests use this).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let var = |k: &str| get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME").unwrap_or_else(|| PathBuf::from("/"));
        let s1_home = var("S1_HOME").unwrap_or_else(|| home.join(".s1"));
        let config_dir = var("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("s1");
        let state_dir = var("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local/state"))
            .join("s1");
        let cache_dir = var("XDG_CACHE_HOME")
            .unwrap_or_else(|| home.join(".cache"))
            .join("s1");
        let socket = var("S1_SOCKET").unwrap_or_else(|| {
            var("XDG_RUNTIME_DIR")
                .map(|d| d.join("s1.sock"))
                .unwrap_or_else(|| state_dir.join("s1.sock"))
        });
        Self {
            s1_home,
            config_dir,
            state_dir,
            cache_dir,
            socket,
        }
    }

    /// Paths rooted in one directory (for tests).
    pub fn under(root: &std::path::Path) -> Self {
        Self {
            s1_home: root.join("s1home"),
            config_dir: root.join("config/s1"),
            state_dir: root.join("state/s1"),
            cache_dir: root.join("cache/s1"),
            socket: root.join("run/s1.sock"),
        }
    }

    pub fn auth_file(&self) -> PathBuf {
        self.s1_home.join("auth.json")
    }
    pub fn models_file(&self) -> PathBuf {
        self.config_dir.join("models.toml")
    }
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.clone()
    }
    /// Written by a running daemon: pid and listener addresses.
    pub fn daemon_file(&self) -> PathBuf {
        self.state_dir.join("daemon.json")
    }
    /// The detached daemon's stderr (startup errors only; logs go to the log file).
    pub fn daemon_stderr(&self) -> PathBuf {
        self.state_dir.join("daemon.stderr")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults_follow_home() {
        let p = Paths::from_lookup(lookup(&[("HOME", "/h")]));
        assert_eq!(p.auth_file(), PathBuf::from("/h/.s1/auth.json"));
        assert_eq!(p.models_file(), PathBuf::from("/h/.config/s1/models.toml"));
        assert_eq!(p.log_dir(), PathBuf::from("/h/.local/state/s1"));
        assert_eq!(p.cache_dir, PathBuf::from("/h/.cache/s1"));
        assert_eq!(p.socket, PathBuf::from("/h/.local/state/s1/s1.sock"));
    }

    #[test]
    fn xdg_and_s1_home_override() {
        let p = Paths::from_lookup(lookup(&[
            ("HOME", "/h"),
            ("S1_HOME", "/k"),
            ("XDG_CONFIG_HOME", "/c"),
            ("XDG_STATE_HOME", "/s"),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
        ]));
        assert_eq!(p.auth_file(), PathBuf::from("/k/auth.json"));
        assert_eq!(p.models_file(), PathBuf::from("/c/s1/models.toml"));
        assert_eq!(p.log_dir(), PathBuf::from("/s/s1"));
        assert_eq!(p.socket, PathBuf::from("/run/user/1/s1.sock"));
    }
}
