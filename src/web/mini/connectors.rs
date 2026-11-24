use super::text;
use crate::{App, mcp::Candidate, security::clip, store};
use anyhow::{Context, Result, ensure};
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

pub(super) async fn add_anything_mcp(app: &App, user: &str, data: &Value) -> Result<Value> {
    let endpoint = text(data, "endpoint", 2048)?;
    let id = app.mcp.prepare_anything_mcp(user, endpoint).await?;
    Ok(json!({"id":id}))
}

pub(super) async fn docblock_oauth(app: &App, user: &str, data: &Value) -> Result<Value> {
    let connector = if data.get("id").is_some() {
        app.mcp.owned(user, text(data, "id", 64)?).await?
    } else {
        app.mcp.prepare_docblock(user).await?
    };
    ensure!(
        crate::mcp::docblock::is_endpoint(&connector.endpoint),
        "Docblock connection unavailable"
    );
    let url = app.mcp.oauth_start(user, &connector.id, None, None).await?;
    Ok(json!({"consent_url":url}))
}

pub(super) async fn docblock_workspaces(app: &App, user: &str, data: &Value) -> Result<Value> {
    let connector = app.mcp.owned(user, text(data, "id", 64)?).await?;
    app.mcp.docblock_workspaces(&connector).await
}

pub(super) async fn docblock_shares(app: &App, user: &str, data: &Value) -> Result<Value> {
    let connector = app.mcp.owned(user, text(data, "id", 64)?).await?;
    let workspace = text(data, "workspace", 36)?;
    uuid::Uuid::parse_str(workspace)?;
    app.mcp.docblock_shares(&connector, workspace).await
}

pub(super) async fn revoke_docblock_share(app: &App, user: &str, data: &Value) -> Result<()> {
    let connector = app.mcp.owned(user, text(data, "id", 64)?).await?;
    let workspace = text(data, "workspace", 36)?;
    let share = text(data, "share", 36)?;
    uuid::Uuid::parse_str(workspace)?;
    uuid::Uuid::parse_str(share)?;
    app.mcp
        .revoke_docblock_share(&connector, workspace, share)
        .await
}

pub(super) async fn approve_docblock_share(app: &App, user: &str, data: &Value) -> Result<()> {
    let request = text(data, "id", 64)?;
    let days = data["expires_in_days"]
        .as_u64()
        .context("invalid link expiry")?;
    store::approve_docblock_share(&app.db, user, request, days).await
}

pub(super) async fn list_tools(app: &App, user: &str, data: &Value) -> Result<Value> {
    let connector = app.mcp.owned(user, text(data, "id", 64)?).await?;
    ensure!(
        connector.auth != "pending",
        "authorize this connection first"
    );
    let source = app.mcp.list_tools(&connector).await?;
    let tools: Vec<_> = source
        .as_array()
        .context("invalid MCP tool list")?
        .iter()
        .filter_map(|tool| {
            let name = tool["name"]
                .as_str()
                .filter(|name| !name.trim().is_empty())?;
            let description = tool["description"].as_str().unwrap_or_default();
            Some(json!({"name":clip(name,128),"description":clip(description,350)}))
        })
        .take(250)
        .collect();
    Ok(json!({"tools":tools,"total":tools.len()}))
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

pub(super) async fn revoke_approval(app: &App, user: &str, data: &Value) -> Result<()> {
    let connector = text(data, "connector", 64)?;
    let tool = text(data, "tool", 128)?;
    store::revoke_mcp_approval(&app.db, user, connector, tool).await?;
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
