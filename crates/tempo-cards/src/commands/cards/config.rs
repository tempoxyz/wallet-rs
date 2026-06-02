//! Card API key configuration.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use tempo_common::error::TempoError;

const CARDS_CONFIG_FILE: &str = "cards.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct CardsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) bridge_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stripe_api_key: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedSecret {
    pub(super) value: String,
    pub(super) source: String,
}

impl CardsConfig {
    pub(super) fn load() -> Result<Self, TempoError> {
        let path = cards_config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)?;
        toml::from_str(&contents).map_err(TempoError::from)
    }

    pub(super) fn save(&self) -> Result<PathBuf, TempoError> {
        let path = cards_config_path()?;
        let parent = path.parent().ok_or_else(|| {
            TempoError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path has no parent directory: {}", path.display()),
            ))
        })?;
        fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;

        let body = toml::to_string_pretty(self)?;
        let contents = format!(
            "# Tempo wallet cards configuration\n\
             # API keys are managed by `tempo wallet cards config`.\n\n\
             {body}"
        );

        let tmp_path = path.with_file_name(format!(".{CARDS_CONFIG_FILE}.{}.tmp", unique_suffix()));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            set_private_file_permissions(&file)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &path)?;
        Ok(path)
    }
}

pub(super) fn cards_config_path() -> Result<PathBuf, TempoError> {
    Ok(tempo_common::tempo_home()?
        .join("wallet")
        .join(CARDS_CONFIG_FILE))
}

pub(super) fn bridge_api_key() -> Result<Option<ResolvedSecret>, TempoError> {
    if let Ok(value) = std::env::var("TEMPO_BRIDGE_API_KEY") {
        if !value.is_empty() {
            return Ok(Some(ResolvedSecret {
                value,
                source: "TEMPO_BRIDGE_API_KEY env var".to_string(),
            }));
        }
    }
    if let Ok(value) = std::env::var("BRIDGE_API_KEY") {
        if !value.is_empty() {
            return Ok(Some(ResolvedSecret {
                value,
                source: "BRIDGE_API_KEY env var".to_string(),
            }));
        }
    }
    let config = CardsConfig::load()?;
    Ok(config.bridge_api_key.map(|value| ResolvedSecret {
        value,
        source: cards_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "$TEMPO_HOME/wallet/cards.toml".to_string()),
    }))
}

pub(super) fn stripe_api_key() -> Result<Option<ResolvedSecret>, TempoError> {
    if let Ok(value) = std::env::var("TEMPO_STRIPE_API_KEY") {
        if !value.is_empty() {
            return Ok(Some(ResolvedSecret {
                value,
                source: "TEMPO_STRIPE_API_KEY env var".to_string(),
            }));
        }
    }
    if let Ok(value) = std::env::var("STRIPE_SECRET_KEY") {
        if !value.is_empty() {
            return Ok(Some(ResolvedSecret {
                value,
                source: "STRIPE_SECRET_KEY env var".to_string(),
            }));
        }
    }
    if let Ok(value) = std::env::var("STRIPE_API_KEY") {
        if !value.is_empty() {
            return Ok(Some(ResolvedSecret {
                value,
                source: "STRIPE_API_KEY env var".to_string(),
            }));
        }
    }
    let config = CardsConfig::load()?;
    Ok(config.stripe_api_key.map(|value| ResolvedSecret {
        value,
        source: cards_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "$TEMPO_HOME/wallet/cards.toml".to_string()),
    }))
}

pub(super) fn mask_secret(secret: &str) -> String {
    if secret.len() <= 8 {
        "***".to_string()
    } else if secret.len() <= 16 {
        format!("{}...{}", &secret[..4], &secret[secret.len() - 4..])
    } else {
        format!("{}...{}", &secret[..12], &secret[secret.len() - 4..])
    }
}

pub(super) fn bridge_environment(api_key: &str) -> &'static str {
    if api_key.starts_with("sk-test") {
        "sandbox"
    } else {
        "production"
    }
}

pub(super) fn stripe_mode(api_key: &str) -> &'static str {
    if api_key.starts_with("sk_test") {
        "test"
    } else {
        "live"
    }
}

fn unique_suffix() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    format!(
        "{}-{}",
        std::process::id(),
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &std::path::Path) -> Result<(), TempoError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &std::path::Path) -> Result<(), TempoError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<(), TempoError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &fs::File) -> Result<(), TempoError> {
    Ok(())
}
