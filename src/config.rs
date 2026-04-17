//! TOML configuration for niri-wallpaper.
//!
//! Config file location: `$XDG_CONFIG_HOME/niri-wallpaper/config.toml`
//! (typically `~/.config/niri-wallpaper/config.toml`).
//!
//! All fields are optional; missing fields use defaults. CLI arguments
//! override config file values.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::dirs_config_home;

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Explicit wallpaper path. If set, disables library rotation.
    pub path: Option<PathBuf>,

    /// Directory containing wallpaper images for rotation.
    /// Default: `$XDG_CONFIG_HOME/niri-wallpaper/library/`
    pub library: Option<PathBuf>,

    /// Cron expression for wallpaper rotation schedule.
    /// Examples: `"*/30 * * * *"` (every 30 min), `"0 * * * *"` (hourly).
    pub schedule: Option<String>,

    pub animation: AnimationConfig,
    pub blur: BlurConfig,
    pub namespaces: NamespaceConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: None,
            library: None,
            schedule: None,
            animation: AnimationConfig::default(),
            blur: BlurConfig::default(),
            namespaces: NamespaceConfig::default(),
        }
    }
}

/// Cross-fade animation settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    /// Duration in milliseconds. 0 = instant switch.
    pub duration_ms: u64,
    /// Easing function.
    pub easing: EasingKind,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            duration_ms: 800,
            easing: EasingKind::EaseInOut,
        }
    }
}

/// Blur settings for the backdrop surface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BlurConfig {
    pub downsample: u32,
    pub radius: u32,
    pub iterations: u32,
}

impl Default for BlurConfig {
    fn default() -> Self {
        Self {
            downsample: 8,
            radius: 6,
            iterations: 3,
        }
    }
}

/// Layer-shell namespace configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct NamespaceConfig {
    pub normal: String,
    pub backdrop: String,
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            normal: "wallpaper-bg".to_string(),
            backdrop: "wallpaper".to_string(),
        }
    }
}

/// Easing function for the cross-fade animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EasingKind {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
}

impl EasingKind {
    /// Map a linear progress `t` in [0, 1] to an eased value in [0, 1].
    pub fn apply(self, t: f32) -> f32 {
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
            Self::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
        }
    }
}

/// Path to the config file.
pub fn config_path() -> Result<PathBuf> {
    Ok(dirs_config_home()?.join("niri-wallpaper").join("config.toml"))
}

/// Load config from the standard path. Returns `Config::default()` if the
/// file does not exist.
pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    load_config_from(&path)
}

/// Load config from a specific path. Returns `Config::default()` if the
/// file does not exist.
pub fn load_config_from(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let config: Config =
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            Ok(config)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::info!("no config file at {}, using defaults", path.display());
            Ok(Config::default())
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Resolve the effective wallpaper path taking config + defaults into account.
/// This is the path to watch via inotify when using `path` mode (not library rotation).
pub fn resolve_wallpaper_path(config: &Config) -> Result<PathBuf> {
    if let Some(ref p) = config.path {
        return Ok(p.clone());
    }
    // Default single-file path (backward compatible).
    Ok(dirs_config_home()?.join("niri-wallpaper").join("current"))
}

/// Resolve the library directory path.
pub fn resolve_library_path(config: &Config) -> Result<PathBuf> {
    if let Some(ref p) = config.library {
        return Ok(p.clone());
    }
    Ok(dirs_config_home()?.join("niri-wallpaper").join("library"))
}
