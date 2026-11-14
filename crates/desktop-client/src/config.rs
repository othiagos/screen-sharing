use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use shared_protocol::{normalize_fingerprint, BitratePreset, TargetResolution};

fn default_server_addr() -> String {
    "127.0.0.1:4433".to_string()
}

fn default_nickname() -> String {
    "Player".to_string()
}

fn default_fps() -> u8 {
    60
}

fn default_true() -> bool {
    true
}

/// Persistent user configuration and TOFU certificate pins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default = "default_server_addr")]
    pub server_addr: String,
    #[serde(default = "default_nickname")]
    pub nickname: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub trusted_fingerprints: HashMap<String, String>,
    #[serde(default)]
    pub resolution: TargetResolution,
    #[serde(default = "default_fps")]
    pub fps: u8,
    #[serde(default)]
    pub bitrate: BitratePreset,
    #[serde(default = "default_true")]
    pub dark_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server_addr: default_server_addr(),
            nickname: default_nickname(),
            auth_token: None,
            trusted_fingerprints: HashMap::new(),
            resolution: TargetResolution::R1080p,
            fps: default_fps(),
            bitrate: BitratePreset::Auto,
            dark_mode: true,
        }
    }
}

impl AppConfig {
    /// Returns the OS-appropriate config file path.
    pub fn default_path() -> PathBuf {
        if let Some(dirs) = ProjectDirs::from("com", "ScreenShare", "ScreenShare") {
            return dirs.config_dir().join("config.json");
        }
        PathBuf::from(".screen-share.json")
    }

    /// Returns the previous pre-rebrand config path for transparent settings migration.
    pub fn legacy_path() -> PathBuf {
        if let Some(dirs) = ProjectDirs::from("com", "ScreenSharing", "desktop-client") {
            return dirs.config_dir().join("config.json");
        }
        PathBuf::from(".screenshare-client.json")
    }

    /// Loads config from `primary`, migrating from `legacy` if `primary` does not yet exist.
    pub fn load_with_migration(primary: &Path, legacy: &Path) -> Self {
        if primary.exists() {
            if let Ok(cfg) = Self::load_from(primary) {
                return cfg;
            }
        }
        if legacy.exists() {
            if let Ok(cfg) = Self::load_from(legacy) {
                let _ = cfg.save_to(primary);
                return cfg;
            }
        }
        Self::default()
    }

    /// Loads config from the default OS path (migrating legacy config if present).
    pub fn load() -> Self {
        Self::load_with_migration(&Self::default_path(), &Self::legacy_path())
    }

    /// Loads config from an explicit file path.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed reading config {}", path.display()))?;
        let mut cfg: Self = serde_json::from_str(&raw)
            .with_context(|| format!("Failed parsing config {}", path.display()))?;
        if ![15, 30, 60].contains(&cfg.fps) {
            cfg.fps = 60;
        }
        Ok(cfg)
    }

    /// Saves config to the default OS path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Atomically persists config JSON to an explicit file path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed creating config dir {}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)
            .with_context(|| format!("Failed writing config {}", path.display()))?;
        Ok(())
    }

    /// Looks up a previously pinned SHA-256 certificate fingerprint for `server_addr`.
    pub fn pinned_fingerprint(&self, server_addr: &str) -> Option<&str> {
        self.trusted_fingerprints
            .get(&server_key(server_addr))
            .map(String::as_str)
    }

    /// Records a normalized SHA-256 certificate fingerprint for `server_addr`.
    pub fn trust_fingerprint(&mut self, server_addr: &str, fingerprint: &str) {
        self.trusted_fingerprints
            .insert(server_key(server_addr), normalize_fingerprint(fingerprint));
    }
}

fn server_key(server_addr: &str) -> String {
    server_addr.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_roundtrip_and_tofu_pins() {
        let dir = std::env::temp_dir().join(format!("ss_cfg_{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("config.json");

        let mut cfg = AppConfig {
            server_addr: "play.example.com:4433".to_string(),
            nickname: "Tester".to_string(),
            resolution: TargetResolution::R1440p,
            fps: 60,
            bitrate: BitratePreset::Mbps20,
            ..Default::default()
        };
        cfg.trust_fingerprint("PLAY.Example.com:4433", "aa:bb:cc:dd");
        cfg.save_to(&path).expect("save config");

        let loaded = AppConfig::load_from(&path).expect("load config");
        assert_eq!(loaded, cfg);
        assert_eq!(
            loaded.pinned_fingerprint("play.example.com:4433"),
            Some("AABBCCDD")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_config_legacy_migration() {
        let dir = std::env::temp_dir().join(format!("ss_mig_{}", uuid::Uuid::new_v4()));
        let legacy = dir.join("old").join("config.json");
        let primary = dir.join("new").join("config.json");

        let mut cfg = AppConfig {
            server_addr: "sfu.example.com:4433".to_string(),
            nickname: "MigratedUser".to_string(),
            ..Default::default()
        };
        cfg.trust_fingerprint("sfu.example.com:4433", "11:22:33:44");
        cfg.save_to(&legacy).expect("save legacy");

        let migrated = AppConfig::load_with_migration(&primary, &legacy);
        assert_eq!(migrated, cfg);
        assert!(primary.exists(), "migrated config should be persisted to new path");

        let _ = fs::remove_dir_all(&dir);
    }
}
