use super::text;
use crate::{App, mcp::Candidate};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) async fn search_mcp(app: &App, data: &Value) -> Result<Value> {
    let search = text(data, "name", 2048)?;
    Ok(serde_json::to_value(
        app.mcp.discover(&app.workers, search).await?,
    )?)
}

pub(super) async fn add_mcp(app: &App, user: &str, data: &Value) -> Result<Value> {
    text(data, "name", 100)?;
    text(data, "endpoint", 2048)?;
    let candidate: Candidate = serde_json::from_value(data.clone())?;
    ensure!(
        data["confirmed"] == true,
        "confirm this connector destination"
    );
    Ok(json!({"id":app.mcp.prepare(user,&candidate).await?}))
}

pub(super) async fn oauth_start(app: &App, user: &str, data: &Value) -> Result<Value> {
    let client = data["client"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 2048);
    let secret = data["secret"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 8192);
    let connector = text(data, "id", 64)?;
    let url = app.mcp.oauth_start(user, connector, client, secret).await?;
    Ok(json!({"consent_url":url}))
}

pub(super) async fn save_token(app: &App, user: &str, data: &Value) -> Result<()> {
    let connector = text(data, "id", 64)?;
    app.mcp
        .save_api(
            user,
            connector,
            text(data, "token", 8192)?,
            data["header"].as_str().unwrap_or("Authorization"),
            data["prefix"].as_str().unwrap_or("Bearer"),
        )
        .await?;
    app.mcp.wake_connected(user, connector).await
}

pub(super) async fn save_public(app: &App, user: &str, data: &Value) -> Result<()> {
    app.mcp.save_public(user, text(data, "id", 64)?).await
}

pub(super) async fn disconnect(app: &App, user: &str, data: &Value) -> Result<()> {
    sqlx::query("DELETE FROM connectors WHERE id=? AND owner=?")
        .bind(text(data, "id", 64)?)
        .bind(user)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub(super) async fn grant(app: &App, user: &str, data: &Value) -> Result<()> {
    app.mcp
        .grant(
            user,
            text(data, "agent", 64)?,
            text(data, "connector", 64)?,
            text(data, "tool", 128)?,
            &data["scope"],
        )
        .await
}

pub(super) async fn revoke(app: &App, user: &str, data: &Value) -> Result<()> {
    sqlx::query(
        "DELETE FROM connector_grants WHERE agent=? AND connector=? AND tool=? AND
         EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)",
    )
    .bind(text(data, "agent", 64)?)
    .bind(text(data, "connector", 64)?)
    .bind(text(data, "tool", 128)?)
    .bind(text(data, "agent", 64)?)
    .bind(user)
    .execute(&app.db)
    .await?;
    Ok(())
}
