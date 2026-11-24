use super::catalog::{openrouter_catalog, valid_model_id, verify_key};
use super::{Models, OpenRouterModel, OpenRouterSettingsError, queue_setup_tasks, valid_key};
use anyhow::{Context, Result, ensure};
use sqlx::FromRow;

const FRESH: &str = "provider IS NULL AND credential IS NULL AND
    openrouter_key IS NULL AND codex_credential IS NULL AND
    display_name IS NULL AND chief_name IS NULL";

#[cfg(test)]
mod tests;

#[derive(FromRow)]
pub(crate) struct OnboardingProfile {
    pub required: bool,
    pub display_name: Option<String>,
    pub chief_name: Option<String>,
}

#[derive(Debug)]
pub(crate) enum OnboardingError {
    InvalidName,
    AlreadyConfigured,
}

impl OnboardingError {
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::InvalidName => "Enter a name of 1 to 64 characters for yourself and your Chief.",
            Self::AlreadyConfigured => {
                "This account is already set up. Refresh TinyBot to continue."
            }
        }
    }
}

impl std::fmt::Display for OnboardingError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(self.message())
    }
}

impl std::error::Error for OnboardingError {}

impl Models {
    pub(crate) async fn onboarding_profile(&self, user: &str) -> Result<OnboardingProfile> {
        Ok(sqlx::query_as(&format!(
            "SELECT ({FRESH}) AS required,display_name,chief_name FROM users WHERE id=?"
        ))
        .bind(user)
        .fetch_one(&self.db)
        .await?)
    }

    pub(crate) async fn onboarding_models(
        &self,
        user: &str,
        key: &str,
    ) -> Result<Vec<OpenRouterModel>> {
        ensure!(
            self.onboarding_profile(user).await?.required,
            OnboardingError::AlreadyConfigured
        );
        ensure!(valid_key(key), OpenRouterSettingsError::InvalidKey);
        let (_, catalog) = tokio::try_join!(verify_key(key), openrouter_catalog(key))?;
        Ok(catalog)
    }

    pub(crate) async fn complete_onboarding(
        &self,
        user: &str,
        display_name: &str,
        chief_name: &str,
        key: &str,
        model: &str,
    ) -> Result<()> {
        let display_name = display_label(display_name).context(OnboardingError::InvalidName)?;
        let chief_name = display_label(chief_name).context(OnboardingError::InvalidName)?;
        ensure!(valid_model_id(model), OpenRouterSettingsError::InvalidModel);
        let catalog = self.onboarding_models(user, key).await?;
        self.complete_verified_onboarding(user, display_name, chief_name, key, model, &catalog)
            .await
    }

    async fn complete_verified_onboarding(
        &self,
        user: &str,
        display_name: &str,
        chief_name: &str,
        key: &str,
        model: &str,
        catalog: &[OpenRouterModel],
    ) -> Result<()> {
        ensure!(
            catalog.iter().any(|entry| entry.id == model),
            OpenRouterSettingsError::InvalidModel
        );
        let encrypted = self
            .vault
            .seal(&format!("openrouter:{user}"), key.as_bytes())?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(&format!(
            "UPDATE users SET provider='openrouter',model=?,openrouter_key=?,display_name=?,chief_name=?
             WHERE id=? AND {FRESH}"
        ))
        .bind(model)
        .bind(encrypted)
        .bind(display_name)
        .bind(chief_name)
        .bind(user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(updated == 1, OnboardingError::AlreadyConfigured);
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) fn display_label(input: &str) -> Option<&str> {
    let label = input.trim();
    (!label.is_empty()
        && input.len() <= 256
        && label.chars().count() <= 64
        && !input.chars().any(char::is_control))
    .then_some(label)
}
