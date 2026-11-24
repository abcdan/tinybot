use super::{Models, OpenRouterSettingsError, openrouter_catalog, valid_model_id};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub(crate) struct BackupModel {
    pub provider: String,
    pub model: String,
}

impl BackupModel {
    pub fn provider_label(&self) -> &'static str {
        if self.provider == "codex" {
            "ChatGPT for Codex"
        } else {
            "OpenRouter"
        }
    }
}

impl Models {
    pub(crate) async fn backup_setting(&self, user: &str) -> Result<Option<BackupModel>> {
        Ok(sqlx::query_as(
            "SELECT backup_provider AS provider,backup_model AS model FROM users
             WHERE id=? AND suspended_at IS NULL AND provider<>backup_provider AND backup_model IS NOT NULL AND
             ((backup_provider='codex' AND codex_credential IS NOT NULL AND codex_model=backup_model)
              OR (backup_provider='openrouter' AND openrouter_key IS NOT NULL))",
        ).bind(user).fetch_optional(&self.db).await?)
    }

    pub(crate) async fn save_backup(&self, user: &str, provider: &str, model: &str) -> Result<()> {
        let changed = match provider {
            "codex" => sqlx::query(
                "UPDATE users SET backup_provider='codex',backup_model=codex_model
                 WHERE id=? AND suspended_at IS NULL AND provider='openrouter'
                   AND codex_model=? AND codex_credential IS NOT NULL",
            )
            .bind(user)
            .bind(model)
            .execute(&self.db)
            .await?
            .rows_affected(),
            "openrouter" => {
                ensure!(valid_model_id(model), OpenRouterSettingsError::InvalidModel);
                let key = self
                    .user_openrouter_key(user)
                    .await?
                    .context(OpenRouterSettingsError::MissingKey)?;
                ensure!(
                    openrouter_catalog(&key.plaintext)
                        .await?
                        .iter()
                        .any(|entry| entry.id == model),
                    OpenRouterSettingsError::InvalidModel
                );
                sqlx::query(
                    "UPDATE users SET backup_provider='openrouter',backup_model=?
                     WHERE id=? AND suspended_at IS NULL AND provider='codex' AND openrouter_key=?",
                )
                .bind(model)
                .bind(user)
                .bind(key.encrypted)
                .execute(&self.db)
                .await?
                .rows_affected()
            }
            _ => 0,
        };
        ensure!(changed == 1, OpenRouterSettingsError::Changed);
        Ok(())
    }

    pub(crate) async fn clear_backup(&self, user: &str) -> Result<()> {
        sqlx::query("UPDATE users SET backup_provider=NULL,backup_model=NULL WHERE id=?")
            .bind(user)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}
