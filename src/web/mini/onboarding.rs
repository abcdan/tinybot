use super::text;
use crate::App;
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub(super) async fn models(app: &App, user: &str, data: &Value) -> Result<Value> {
    let key = text(data, "key", 8192)?;
    Ok(json!({"models":app.models.onboarding_models(user, key).await?}))
}

pub(super) async fn complete(app: &App, user: &str, data: &Value) -> Result<()> {
    let display_name = data["display_name"]
        .as_str()
        .context("enter your display name")?;
    let chief_name = data["chief_name"]
        .as_str()
        .context("enter a name for your Chief")?;
    app.models
        .complete_onboarding(
            user,
            display_name,
            chief_name,
            text(data, "key", 8192)?,
            text(data, "model", 150)?,
        )
        .await
}
