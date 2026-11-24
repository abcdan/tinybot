use super::{
    Models, OpenRouterSettingsError, openrouter_catalog, queue_setup_tasks, valid_key,
    valid_model_id,
};
use crate::security::hash;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, SqliteConnection};

pub(crate) struct ModelSelection<'a> {
    pub provider: &'static str,
    pub model: &'a str,
}

impl<'a> ModelSelection<'a> {
    pub(crate) fn parse(value: &'a str, current: &'a ModelPreferences) -> Result<Self> {
        if value == "codex" {
            let model = current
                .codex_model
                .as_deref()
                .context("Connect ChatGPT for Codex in Settings & forms before selecting it.")?;
            return Ok(Self {
                provider: "codex",
                model,
            });
        }
        let model = value
            .strip_prefix("openrouter:")
            .filter(|model| valid_model_id(model))
            .context("Enter codex or openrouter:provider/model.")?;
        ensure!(
            current.openrouter_identity.is_some(),
            OpenRouterSettingsError::MissingKey
        );
        Ok(Self {
            provider: "openrouter",
            model,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ModelPreferences {
    provider: Option<String>,
    model: Option<String>,
    codex_model: Option<String>,
    backup_provider: Option<String>,
    backup_model: Option<String>,
    openrouter_identity: Option<String>,
    codex_identity: Option<String>,
}

#[derive(FromRow)]
struct ModelPreferenceRow {
    provider: Option<String>,
    model: Option<String>,
    codex_model: Option<String>,
    backup_provider: Option<String>,
    backup_model: Option<String>,
    openrouter_key: Option<String>,
    codex_credential: Option<String>,
}

impl ModelPreferences {
    pub(crate) async fn read(db: &mut SqliteConnection, user: &str) -> Result<Self> {
        let row: ModelPreferenceRow = sqlx::query_as(
            "SELECT provider,model,backup_provider,backup_model,
                    CASE WHEN codex_credential IS NOT NULL THEN codex_model
                         WHEN provider='codex' AND credential IS NOT NULL THEN model END AS codex_model,
                    COALESCE(CASE WHEN provider='openrouter' THEN credential END,openrouter_key) AS openrouter_key,
                    COALESCE(codex_credential,CASE WHEN provider='codex' THEN credential END) AS codex_credential
             FROM users WHERE id=? AND suspended_at IS NULL",
        )
        .bind(user)
        .fetch_one(db)
        .await?;
        Ok(Self {
            provider: row.provider,
            model: row.model,
            codex_model: row.codex_model,
            backup_provider: row.backup_provider,
            backup_model: row.backup_model,
            openrouter_identity: row.openrouter_key.as_deref().map(hash),
            codex_identity: row.codex_credential.as_deref().map(hash),
        })
    }

    pub(crate) fn task_setting(&self) -> String {
        match (self.provider.as_deref(), self.model.as_deref()) {
            (Some("codex"), Some(_)) => "codex".into(),
            (Some("openrouter"), Some(model)) => format!("openrouter:{model}"),
            _ => String::new(),
        }
    }

    pub(crate) fn backup_setting(&self) -> String {
        match (
            self.backup_provider.as_deref(),
            self.backup_model.as_deref(),
        ) {
            (Some("codex"), Some(model)) if self.codex_model.as_deref() == Some(model) => {
                "codex".into()
            }
            (Some("codex"), Some(model)) => format!("codex:{model}"),
            (Some("openrouter"), Some(model)) => format!("openrouter:{model}"),
            _ => "off".into(),
        }
    }

    pub(crate) fn public(&self) -> Value {
        let backup_available = self.provider != self.backup_provider
            && match (
                self.backup_provider.as_deref(),
                self.backup_model.as_deref(),
            ) {
                (Some("codex"), Some(model)) => self.codex_model.as_deref() == Some(model),
                (Some("openrouter"), Some(_)) => self.openrouter_identity.is_some(),
                _ => false,
            };
        json!({
            "task_provider":self.provider,
            "task_model":self.model,
            "backup_provider":self.backup_provider,
            "backup_model":self.backup_model,
            "backup_available":backup_available,
            "openrouter_connected":self.openrouter_identity.is_some(),
            "paired_codex_model":self.codex_model,
        })
    }

    pub(crate) fn describe(&self, option: &str) -> String {
        match option {
            "codex" => self.codex_model.as_deref().map_or_else(
                || "ChatGPT via Codex".into(),
                |model| format!("ChatGPT via Codex · {model}"),
            ),
            "off" => "Off".into(),
            "" => "Not set".into(),
            _ => {
                if let Some(model) = option.strip_prefix("openrouter:") {
                    format!("OpenRouter · {model}")
                } else if let Some(model) = option.strip_prefix("codex:") {
                    format!("ChatGPT via Codex · {model}")
                } else {
                    option.to_owned()
                }
            }
        }
    }

    pub(crate) async fn apply(
        &self,
        db: &mut SqliteConnection,
        user: &str,
        task: Option<&str>,
        backup: Option<&str>,
    ) -> Result<()> {
        if let Some(task) = task {
            let selection = ModelSelection::parse(task, self)?;
            // Preserve a pre-migration connection if switching away from it.
            // OpenRouter's normal credential load re-seals tokens in the older binding.
            sqlx::query(
                "UPDATE users SET provider=?,model=?,credential=NULL,
                        openrouter_key=COALESCE(CASE WHEN provider='openrouter' THEN credential END,openrouter_key),
                        codex_credential=COALESCE(codex_credential,CASE WHEN provider='codex' THEN credential END),
                        codex_model=COALESCE(codex_model,CASE WHEN provider='codex' AND credential IS NOT NULL THEN model END)
                 WHERE id=?",
            )
                .bind(selection.provider)
                .bind(selection.model)
                .bind(user)
                .execute(&mut *db)
                .await?;
            queue_setup_tasks(&mut *db, user).await?;
        }
        if let Some(backup) = backup {
            let selection = (backup != "off")
                .then(|| ModelSelection::parse(backup, self))
                .transpose()?;
            sqlx::query("UPDATE users SET backup_provider=?,backup_model=? WHERE id=?")
                .bind(selection.as_ref().map(|value| value.provider))
                .bind(selection.map(|value| value.model))
                .bind(user)
                .execute(db)
                .await?;
        }
        Ok(())
    }
}

impl Models {
    async fn settings_openrouter_key(&self, user: &str) -> Result<String> {
        let (stored, legacy) = self.openrouter_material(user).await?;
        if let Some(ciphertext) = stored.as_deref().filter(|_| legacy.is_none())
            && let Ok(plaintext) = self.vault.open(&format!("openrouter:{user}"), ciphertext)
        {
            return Ok(String::from_utf8(plaintext)?);
        }
        let ciphertext = legacy
            .as_deref()
            .or(stored.as_deref())
            .context(OpenRouterSettingsError::MissingKey)?;
        let plaintext = String::from_utf8(self.vault.open(&format!("model:{user}"), ciphertext)?)?;
        ensure!(valid_key(&plaintext), OpenRouterSettingsError::InvalidKey);
        Ok(plaintext)
    }

    pub(crate) async fn validate_settings_models(
        &self,
        user: &str,
        selections: &[ModelSelection<'_>],
    ) -> Result<()> {
        if selections
            .iter()
            .any(|entry| entry.provider == "openrouter")
        {
            let key = self.settings_openrouter_key(user).await?;
            let catalog = openrouter_catalog(&key).await?;
            ensure!(
                selections
                    .iter()
                    .filter(|entry| entry.provider == "openrouter")
                    .all(|entry| catalog.iter().any(|model| model.id == entry.model)),
                OpenRouterSettingsError::InvalidModel
            );
        }
        Ok(())
    }
}
