mod backup;
mod catalog;
mod onboarding;
mod server_tools;
#[cfg(test)]
mod tests;

use super::Models;
use crate::siwc::Profile;
use anyhow::{Context, Result, ensure};
pub(crate) use backup::BackupModel;
pub use catalog::OpenRouterModel;
use catalog::{openrouter_catalog, valid_model_id, verify_key};
pub(crate) use onboarding::OnboardingError;
use serde::Serialize;
use sqlx::{Sqlite, Transaction};

#[derive(Serialize)]
pub struct ModelConnections {
    pub openrouter_key: bool,
    pub codex_model: Option<String>,
}

#[derive(Debug)]
pub enum OpenRouterSettingsError {
    MissingKey,
    InvalidKey,
    InvalidModel,
    Unavailable,
    Changed,
}

impl std::fmt::Display for OpenRouterSettingsError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::MissingKey => "Save an OpenRouter API key first.",
            Self::InvalidKey => "OpenRouter rejected this key. Use a standard OpenRouter API key.",
            Self::InvalidModel => "Choose a tool-capable model available with this OpenRouter key.",
            Self::Unavailable => {
                "OpenRouter model settings could not be verified. Try again shortly."
            }
            Self::Changed => "The OpenRouter key changed. Reload the model list and choose again.",
        })
    }
}

impl std::error::Error for OpenRouterSettingsError {}

pub(super) struct UserKey {
    pub(super) plaintext: String,
    encrypted: String,
}

impl Models {
    pub async fn connections(&self, user: &str) -> Result<ModelConnections> {
        let (openrouter_key, codex_model): (bool, Option<String>) = sqlx::query_as(
            "SELECT openrouter_key IS NOT NULL OR (provider IS 'openrouter' AND credential IS NOT NULL),
                    CASE WHEN codex_credential IS NOT NULL THEN codex_model
                         WHEN provider IS 'codex' AND credential IS NOT NULL THEN model END
             FROM users WHERE id=?",
        )
        .bind(user)
        .fetch_one(&self.db)
        .await?;
        Ok(ModelConnections {
            openrouter_key,
            codex_model,
        })
    }

    pub(super) async fn user_openrouter_key(&self, user: &str) -> Result<Option<UserKey>> {
        // The Chief and Mercury can load the same legacy key concurrently.
        // The CAS loser reads the winner's new ciphertext rather than aborting the task.
        for _ in 0..2 {
            let (stored, legacy): (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT openrouter_key, CASE WHEN provider='openrouter' THEN credential END
                 FROM users WHERE id=?",
            )
            .bind(user)
            .fetch_one(&self.db)
            .await?;
            let binding = format!("openrouter:{user}");
            if legacy.is_none()
                && let Some(encrypted) = &stored
                && let Ok(plain) = self.vault.open(&binding, encrypted)
            {
                return Ok(Some(UserKey {
                    plaintext: String::from_utf8(plain)?,
                    encrypted: encrypted.clone(),
                }));
            }
            // The migration transfers old task ciphertext without key material.
            // Read that binding only for a legacy OpenRouter key and atomically re-seal it.
            let Some(old) = legacy.as_ref().or(stored.as_ref()) else {
                return Ok(None);
            };
            let plaintext = String::from_utf8(self.vault.open(&format!("model:{user}"), old)?)?;
            ensure!(
                valid_key(&plaintext),
                "invalid legacy OpenRouter credential"
            );
            let encrypted = self.vault.seal(&binding, plaintext.as_bytes())?;
            let updated = sqlx::query(
                "UPDATE users SET openrouter_key=?, credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END
                 WHERE id=? AND openrouter_key IS ? AND (CASE WHEN provider='openrouter' THEN credential END) IS ?",
            )
            .bind(&encrypted)
            .bind(user)
            .bind(&stored)
            .bind(&legacy)
            .execute(&self.db)
            .await?
            .rows_affected();
            if updated == 1 {
                return Ok(Some(UserKey {
                    plaintext,
                    encrypted,
                }));
            }
        }
        Err(OpenRouterSettingsError::Changed.into())
    }

    pub async fn save_openrouter_key(&self, user: &str, key: &str) -> Result<Vec<OpenRouterModel>> {
        ensure!(valid_key(key), OpenRouterSettingsError::InvalidKey);
        let (_, catalog) = tokio::try_join!(verify_key(key), openrouter_catalog(key))?;
        self.save_verified_openrouter_key(user, key).await?;
        Ok(catalog)
    }

    async fn save_verified_openrouter_key(&self, user: &str, key: &str) -> Result<()> {
        let encrypted = self
            .vault
            .seal(&format!("openrouter:{user}"), key.as_bytes())?;
        sqlx::query(
            "UPDATE users SET openrouter_key=?, credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END WHERE id=?",
        )
        .bind(encrypted)
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn openrouter_models(&self, user: &str) -> Result<Vec<OpenRouterModel>> {
        let key = self
            .user_openrouter_key(user)
            .await?
            .context(OpenRouterSettingsError::MissingKey)?;
        openrouter_catalog(&key.plaintext).await
    }

    pub async fn select_openrouter_model(&self, user: &str, model: &str) -> Result<()> {
        ensure!(valid_model_id(model), OpenRouterSettingsError::InvalidModel);
        let key = self
            .user_openrouter_key(user)
            .await?
            .context(OpenRouterSettingsError::MissingKey)?;
        let catalog = openrouter_catalog(&key.plaintext).await?;
        ensure!(
            catalog.iter().any(|entry| entry.id == model),
            OpenRouterSettingsError::InvalidModel
        );
        self.activate_openrouter(user, model, &key).await
    }

    async fn activate_openrouter(&self, user: &str, model: &str, key: &UserKey) -> Result<()> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE users SET provider='openrouter', model=?, credential=NULL
             WHERE id=? AND openrouter_key=?",
        )
        .bind(model)
        .bind(user)
        .bind(&key.encrypted)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(updated == 1, OpenRouterSettingsError::Changed);
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn connect_codex(&self, user: &str, model: &str, profile: &Profile) -> Result<()> {
        ensure!(
            !model.is_empty()
                && model.len() <= 150
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
            "invalid Codex model"
        );
        let encrypted = self
            .vault
            .seal(&format!("model:{user}"), &serde_json::to_vec(profile)?)?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(
            "UPDATE users SET provider='codex', model=?, codex_model=?, codex_credential=?, credential=NULL
             WHERE id=? AND suspended_at IS NULL",
        )
        .bind(model)
        .bind(model)
        .bind(encrypted)
        .bind(user)
        .execute(&mut *tx)
        .await?;
        ensure!(changed.rows_affected() == 1, "account unavailable");
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn select_codex(&self, user: &str) -> Result<()> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE users SET provider='codex', model=codex_model, credential=NULL
             WHERE id=? AND codex_model IS NOT NULL AND codex_credential IS NOT NULL",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(updated == 1, "pair ChatGPT for Codex first");
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn disconnect_model(&self, user: &str) -> Result<()> {
        sqlx::query("UPDATE users SET provider=NULL,model=NULL WHERE id=?")
            .bind(user)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn disconnect_openrouter(&self, user: &str) -> Result<()> {
        sqlx::query(
            "UPDATE users SET openrouter_key=NULL,
             model=CASE WHEN provider='openrouter' THEN NULL ELSE model END,
             credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END,
             provider=CASE WHEN provider='openrouter' THEN NULL ELSE provider END
             WHERE id=?",
        )
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn disconnect_codex(&self, user: &str) -> Result<()> {
        sqlx::query(
            "UPDATE users SET codex_model=NULL,codex_credential=NULL,
             model=CASE WHEN provider='codex' THEN NULL ELSE model END,
             credential=CASE WHEN provider='codex' THEN NULL ELSE credential END,
             provider=CASE WHEN provider='codex' THEN NULL ELSE provider END
             WHERE id=?",
        )
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

async fn queue_setup_tasks(tx: &mut Transaction<'_, Sqlite>, user: &str) -> Result<()> {
    sqlx::query("UPDATE tasks SET state='queued' WHERE user=? AND state='setup'")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn valid_key(key: &str) -> bool {
    key.strip_prefix("sk-or-").is_some_and(|key| {
        !key.is_empty()
            && key.len() <= 8192
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    })
}
