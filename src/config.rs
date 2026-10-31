//! `~/.config/picha/config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The name phones see; empty uses the hostname.
    pub name: String,
    /// Where received files go; empty uses XDG_DOWNLOAD_DIR.
    pub dir: String,
    /// "es" or "en"; empty follows $LANG.
    pub lang: String,
    /// TCP port phones connect to; 0 uses picha's fixed default.
    pub port: u16,
    /// Phones whose transfers are accepted without asking, by the name they announce.
    pub trusted: Vec<String>,
    /// The lumbre's Wi-Fi password, made the first time it's lit.
    pub lumbre_password: String,
}

/// Fixed so restarts keep the address phones have cached.
pub const DEFAULT_PORT: u16 = 47_374;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
}

impl Config {
    fn path() -> PathBuf {
        config_dir().join("picha/config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().expect("config path has a parent"))?;
        std::fs::write(path, serde_json::to_string_pretty(self).expect("config serializes") + "\n")
    }

    pub fn is_trusted(&self, phone: &str) -> bool {
        self.trusted.iter().any(|t| t == phone)
    }

    pub fn hostname() -> String {
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "linux".into())
    }

    pub fn name(&self) -> String {
        if self.name.is_empty() { Self::hostname() } else { self.name.clone() }
    }

    /// XDG_DOWNLOAD_DIR from user-dirs.dirs, else ~/Downloads.
    pub fn dir(&self) -> PathBuf {
        if !self.dir.is_empty() {
            return PathBuf::from(self.dir.replacen('~', &home().to_string_lossy(), 1));
        }
        std::fs::read_to_string(config_dir().join("user-dirs.dirs"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR="))
                    .map(|v| PathBuf::from(v.trim_matches('"').replace("$HOME", &home().to_string_lossy())))
            })
            .unwrap_or_else(|| home().join("Downloads"))
    }

    pub fn port(&self) -> u16 {
        if self.port == 0 { DEFAULT_PORT } else { self.port }
    }

    pub fn spanish(&self) -> bool {
        match self.lang.as_str() {
            "es" => true,
            "en" => false,
            _ => std::env::var("LANG").is_ok_and(|l| l.starts_with("es")),
        }
    }
}
