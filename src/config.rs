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

/// `~/Library/Application Support/asb-tui/config.json` su macOS.
/// Il percorso legacy resta invariato anche in az-servicebus per mantenere
/// compatibilità con i profili già salvati, senza richiedere migrazioni.
pub fn config_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().context("Impossibile determinare la home directory")?;
    Ok(base.config_dir().join("asb-tui").join("config.json"))
}

/// Caricamento sincrono: usato solo all'avvio, prima di entrare nel loop UI.
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Lettura di {} fallita", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("JSON non valido in {}", path.display()))
}

/// Salvataggio asincrono: eseguito dal worker, mai dal thread UI.
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
        // Il file contiene segreti: solo l'utente corrente può leggerlo.
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}
