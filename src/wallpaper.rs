//! Wallpaper library scanning, usage tracking, and weighted random selection.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rand::distr::weighted::WeightedIndex;
use rand::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// XDG state directory
// ---------------------------------------------------------------------------

/// `$XDG_STATE_HOME` (default `~/.local/state`).
fn dirs_state_home() -> Result<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(x));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("neither XDG_STATE_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

fn state_dir() -> Result<PathBuf> {
    Ok(dirs_state_home()?.join("niri-wallpaper"))
}

fn usage_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("usage.toml"))
}

// ---------------------------------------------------------------------------
// Library scanning
// ---------------------------------------------------------------------------

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// Scan `dir` for image files. Returns an empty vec if the directory does not
/// exist.
pub fn scan_library(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("cannot read library dir {}: {}", dir.display(), e);
            }
            return Vec::new();
        }
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let ext = path.extension().and_then(OsStr::to_str)?.to_ascii_lowercase();
            if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort for deterministic ordering (used as key in usage tracking).
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Usage tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageData {
    #[serde(default)]
    usage: HashMap<String, u64>,
}

/// Persistent usage tracker. Loads/saves from `$XDG_STATE_HOME/niri-wallpaper/usage.toml`.
pub struct UsageTracker {
    data: UsageData,
}

impl UsageTracker {
    /// Load usage data from disk. Returns a fresh tracker if the file doesn't
    /// exist or is unparseable.
    pub fn load() -> Self {
        let data = (|| -> Result<UsageData> {
            let path = usage_path()?;
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(UsageData::default());
                }
                Err(e) => return Err(e).context("read usage file"),
            };
            toml::from_str(&text).context("parse usage file")
        })();

        match data {
            Ok(d) => Self { data: d },
            Err(e) => {
                log::warn!("failed to load usage data, starting fresh: {:#}", e);
                Self { data: UsageData::default() }
            }
        }
    }

    /// Save usage data to disk.
    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            log::error!("failed to save usage data: {:#}", e);
        }
    }

    fn try_save(&self) -> Result<()> {
        let path = usage_path()?;
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let text = toml::to_string_pretty(&self.data).context("serialize usage data")?;
        std::fs::write(&path, text)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Pick a random wallpaper from `candidates`, weighted by inverse usage
    /// count. Excludes `current` (if set) to avoid repeats.
    ///
    /// Returns the chosen path, or `None` if no candidates are available.
    pub fn pick(
        &mut self,
        candidates: &[PathBuf],
        current: Option<&Path>,
    ) -> Option<PathBuf> {
        // Filter out the currently displayed wallpaper.
        let eligible: Vec<&PathBuf> = candidates
            .iter()
            .filter(|p| {
                current.map_or(true, |c| {
                    // Compare canonical paths if possible, fall back to direct comparison.
                    let p_canon = std::fs::canonicalize(p).ok();
                    let c_canon = std::fs::canonicalize(c).ok();
                    match (p_canon, c_canon) {
                        (Some(a), Some(b)) => a != b,
                        _ => p.as_path() != c,
                    }
                })
            })
            .collect();

        if eligible.is_empty() {
            // If only one wallpaper in the library, allow re-selecting it.
            if candidates.len() == 1 {
                return Some(candidates[0].clone());
            }
            return None;
        }

        // Compute weights: 1.0 / (count + 1).
        let weights: Vec<f64> = eligible
            .iter()
            .map(|p| {
                let key = filename_key(p);
                let count = self.data.usage.get(&key).copied().unwrap_or(0);
                1.0 / (count as f64 + 1.0)
            })
            .collect();

        let dist = match WeightedIndex::new(&weights) {
            Ok(d) => d,
            Err(e) => {
                log::error!("weighted selection failed: {}", e);
                return None;
            }
        };

        let mut rng = rand::rng();
        let idx = dist.sample(&mut rng);
        let chosen = eligible[idx].clone();

        // Increment usage count.
        let key = filename_key(&chosen);
        *self.data.usage.entry(key).or_insert(0) += 1;
        self.save();

        Some(chosen)
    }
}

fn filename_key(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Current wallpaper symlink
// ---------------------------------------------------------------------------

/// Atomically update the `current` symlink in the config directory to point
/// to `target`. Uses the rename trick (create temp symlink, rename over
/// `current`) to avoid a window where `current` doesn't exist.
pub fn update_current_symlink(target: &Path) -> Result<()> {
    let config_dir = crate::dirs_config_home()?.join("niri-wallpaper");
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("create {}", config_dir.display()))?;

    let symlink_path = config_dir.join("current");
    let tmp_path = config_dir.join(".current.tmp");

    // Remove stale temp symlink if it exists.
    let _ = std::fs::remove_file(&tmp_path);

    std::os::unix::fs::symlink(target, &tmp_path)
        .with_context(|| format!("symlink {} -> {}", tmp_path.display(), target.display()))?;
    std::fs::rename(&tmp_path, &symlink_path).with_context(|| {
        format!(
            "rename {} -> {}",
            tmp_path.display(),
            symlink_path.display()
        )
    })?;

    log::debug!("updated current symlink → {}", target.display());
    Ok(())
}
