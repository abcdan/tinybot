use crate::model::{
    Models,
    images::{IMAGE_MODELS, ImageModel, resolve_image_model},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::SqliteConnection;

#[derive(Serialize)]
pub(crate) struct ImageSettings {
    selected: &'static str,
    choices: &'static [ImageModel],
}

impl Models {
    pub(crate) async fn image_model(&self, user: &str) -> Result<&'static str> {
        let saved: Option<String> =
            sqlx::query_scalar("SELECT image_model FROM users WHERE id=? AND suspended_at IS NULL")
                .bind(user)
                .fetch_one(&self.db)
                .await?;
        Ok(resolve_image_model(saved.as_deref()))
    }

    pub(crate) async fn image_settings(&self, user: &str) -> Result<ImageSettings> {
        Ok(ImageSettings {
            selected: self.image_model(user).await?,
            choices: IMAGE_MODELS,
        })
    }

    pub(crate) async fn image_model_available(&self, user: &str) -> Result<bool> {
        Ok(self.connections(user).await?.openrouter_key)
    }
}

pub(crate) async fn write_image_model(
    db: &mut SqliteConnection,
    user: &str,
    model: &str,
) -> Result<()> {
    ensure!(
        IMAGE_MODELS.iter().any(|entry| entry.id == model),
        "Choose an available image model."
    );
    let updated = sqlx::query("UPDATE users SET image_model=? WHERE id=? AND suspended_at IS NULL")
        .bind(model)
        .bind(user)
        .execute(db)
        .await?
        .rows_affected();
    ensure!(updated == 1, "Image model settings are unavailable.");
    Ok(())
}
