use super::text;
use crate::{
    App,
    security::{hash, now, token},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) async fn save_openrouter_key(app: &App, user: &str, data: &Value) -> Result<Value> {
    let models = app
        .models
        .save_openrouter_key(user, text(data, "key", 8192)?)
        .await?;
    Ok(json!({"models":models}))
}

pub(super) async fn openrouter_models(app: &App, user: &str) -> Result<Value> {
    Ok(json!({"models":app.models.openrouter_models(user).await?}))
}

pub(super) async fn select_openrouter_model(app: &App, user: &str, data: &Value) -> Result<()> {
    app.models
        .select_openrouter_model(user, text(data, "model", 150)?)
        .await
}

pub(super) async fn save_backup_model(app: &App, user: &str, data: &Value) -> Result<()> {
    app.models
        .save_backup(user, text(data, "provider", 20)?, text(data, "model", 150)?)
        .await
}

pub(super) async fn pair_codex(app: &App, user: &str, data: &Value) -> Result<Value> {
    let model = text(data, "model", 150)?;
    ensure!(
        model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
        "invalid Codex model"
    );
    let pairing = token();
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM pending WHERE user=? AND kind='codex-pair'")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO pending(id,user,kind,payload,expires) VALUES(?,?,'codex-pair',?,?)")
        .bind(hash(&pairing))
        .bind(user)
        .bind(model)
        .bind(now() + 900)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"code":pairing,"command":format!("tinybot codex connect {}",app.origin)}))
}
