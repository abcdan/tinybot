use super::{sharing, text};
use crate::{
    App,
    store::memory::{self, MemoryScope},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn selection(data: &Value) -> Result<(MemoryScope, Option<&str>)> {
    match data["scope"].as_str() {
        Some("private") if data["agent"].is_null() => Ok((MemoryScope::Chief, None)),
        Some("group") if data["agent"].is_null() => Ok((MemoryScope::Group, None)),
        Some("agent") => Ok((MemoryScope::Helper, Some(text(data, "agent", 64)?))),
        _ => anyhow::bail!("choose a memory scope"),
    }
}

pub(super) async fn list(app: &App, user: &str, data: &Value) -> Result<Value> {
    let (scope, agent) = selection(data)?;
    let before = data
        .get("before")
        .map(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .context("invalid memory cursor")
        })
        .transpose()?;
    let (entries, next_before) = memory::entries(&app.db, user, scope, agent, before).await?;
    let entries: Vec<_> = entries.into_iter().map(|entry| json!({
        "id":entry.id,"agent":entry.agent,"scope":data["scope"],"body":entry.body,"created_at":entry.created_at
    })).collect();
    Ok(json!({"entries":entries,"next_before":next_before}))
}

pub(super) async fn save(app: &App, user: &str, data: &Value) -> Result<()> {
    let (scope, agent) = selection(data)?;
    memory::save(&app.db, user, scope, agent, text(data, "body", 1200)?).await?;
    Ok(())
}

pub(super) async fn remove(app: &App, user: &str, data: &Value) -> Result<()> {
    let id = text(data, "id", 64)?;
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "invalid memory ID");
    if !memory::remove(&app.db, user, id).await? {
        sharing::delete_published_memory(app, user, id).await?;
    }
    Ok(())
}
