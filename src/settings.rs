//! AI provider settings. The API key is stored outside the project directory
//! with user-only permissions, is never logged, and is never returned to the
//! UI (PRD §7.1, §13, §16.3).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OFFLINE: &str = "offline";

pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AiSettings {
    pub provider: String,
    #[serde(default)]
    pub model: String,
    /// Never serialized into API responses — see `public()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings { provider: PROVIDER_OPENAI.into(), model: String::new(), api_key: None }
    }
}

/// The only settings shape that ever leaves the backend.
#[derive(Serialize, Clone, Debug)]
pub struct PublicSettings {
    pub provider: String,
    pub model: String,
    pub connected: bool,
}

impl AiSettings {
    pub fn effective_model(&self) -> String {
        if !self.model.trim().is_empty() {
            return self.model.trim().to_string();
        }
        match self.provider.as_str() {
            PROVIDER_ANTHROPIC => DEFAULT_ANTHROPIC_MODEL.into(),
            _ => DEFAULT_OPENAI_MODEL.into(),
        }
    }

    pub fn connected(&self) -> bool {
        self.provider == PROVIDER_OFFLINE
            || self.api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false)
    }

    pub fn public(&self) -> PublicSettings {
        PublicSettings {
            provider: self.provider.clone(),
            model: self.effective_model(),
            connected: self.connected(),
        }
    }
}

fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

pub fn load(data_dir: &Path) -> AiSettings {
    let path = settings_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => AiSettings::default(),
    }
}

pub fn save(data_dir: &Path, settings: &AiSettings) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = settings_path(data_dir);
    let json = serde_json::to_vec_pretty(settings)?;
    std::fs::write(&path, json)?;
    // User-only read/write (PRD §7.1).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(())
}
