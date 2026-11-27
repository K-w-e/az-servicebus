use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub connection_string: String,
}

impl Config {
    pub fn active(&self) -> Option<&Profile> {
        let name = self.active_profile.as_deref()?;
        self.profiles.iter().find(|p| p.name == name)
    }
}

/// `~/Library/Application Support/asb-tui/config.json` on macOS.
/// The legacy path remains unchanged in az-servicebus to preserve existing
/// saved profiles without requiring migrations.
pub fn config_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("Could not determine the home directory")?;
    Ok(base.config_dir().join("asb-tui").join("config.json"))
}

/// Synchronous loading: used only at startup, before entering the UI loop.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("Invalid JSON in {}", path.display()))
}

/// Asynchronous saving: performed by the worker, never by the UI thread.
pub async fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    tokio::fs::write(&path, json).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The file contains secrets: only the current user can read it.
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}
