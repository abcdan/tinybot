use super::text;
use crate::{
    App,
    security::{hash, id, now, token},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) async fn invite_agent(app: &App, user: &str, data: &Value) -> Result<Value> {
    let agent = text(data, "agent", 64)?;
    let invitation = token();
    let affected = sqlx::query(
        "INSERT INTO invitations(token,issuer,agent,expires) SELECT ?,owner,id,? FROM agents
         WHERE id=? AND owner=?",
    )
    .bind(hash(&invitation))
    .bind(now() + 86400)
    .bind(agent)
    .bind(user)
    .execute(&app.db)
    .await?
    .rows_affected();
    ensure!(affected == 1, "agent unavailable");
    Ok(json!({"command":format!("/start {invitation}"),"expires_in":86400}))
}

pub(super) async fn revoke_member(app: &App, user: &str, data: &Value) -> Result<()> {
    let agent = text(data, "agent", 64)?;
    let member = text(data, "member", 24)?;
    sqlx::query(
        "DELETE FROM grants WHERE agent=? AND member=?
         AND EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)",
    )
    .bind(agent)
    .bind(member)
    .bind(agent)
    .bind(user)
    .execute(&app.db)
    .await?;
    Ok(())
}

pub(super) async fn publish_memory(app: &App, user: &str, data: &Value) -> Result<()> {
    let agent = text(data, "agent", 64)?;
    let body = text(data, "body", 1200)?;
    let changed = sqlx::query(
        "INSERT INTO memory(id,owner,agent,body) SELECT ?,owner,id,? FROM agents WHERE id=? AND owner=?",
    )
    .bind(id())
    .bind(body)
    .bind(agent)
    .bind(user)
    .execute(&app.db)
    .await?
    .rows_affected();
    ensure!(changed == 1, "agent unavailable");
    Ok(())
}

pub(super) async fn delete_published_memory(app: &App, user: &str, entry: &str) -> Result<()> {
    sqlx::query("DELETE FROM memory WHERE id=? AND owner=?")
        .bind(entry)
        .bind(user)
        .execute(&app.db)
        .await?;
    Ok(())
}
