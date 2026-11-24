use super::text;
use crate::{
    App,
    store::{invitations, shared_memory},
};
use anyhow::Result;
use serde_json::{Value, json};

pub(super) async fn list(app: &App, user: &str) -> Result<Value> {
    let (pending, guests) = tokio::try_join!(
        invitations::pending(&app.db, user),
        invitations::guests(&app.db, user)
    )?;
    Ok(json!({"pending":pending,"guests":guests}))
}

pub(super) async fn create(app: &App, user: &str) -> Result<Value> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    let invite = invitations::create(&mut tx, user).await?;
    tx.commit().await?;
    Ok(serde_json::to_value(invite)?)
}

pub(super) async fn revoke(app: &App, user: &str, data: &Value) -> Result<()> {
    invitations::revoke(&app.db, user, text(data, "id", 36)?).await
}

pub(super) async fn suspend(app: &App, user: &str, data: &Value, suspended: bool) -> Result<()> {
    invitations::set_suspended(&app.db, user, text(data, "guest", 24)?, suspended).await
}

pub(super) async fn offer_shared_memory(app: &App, user: &str, data: &Value) -> Result<()> {
    shared_memory::invite(&app.db, user, text(data, "guest", 24)?).await
}

pub(super) async fn pairs(app: &App, user: &str) -> Result<Value> {
    Ok(json!({"pairs":shared_memory::pairs(&app.db, user).await?}))
}

pub(super) async fn set_joined(app: &App, user: &str, data: &Value, joined: bool) -> Result<()> {
    shared_memory::set_joined(&app.db, user, text(data, "pair", 36)?, joined).await
}

pub(super) async fn notes(app: &App, user: &str, data: &Value) -> Result<Value> {
    Ok(json!({"notes":shared_memory::notes(&app.db, user, text(data,"pair",36)?).await?}))
}

pub(super) async fn publish(app: &App, user: &str, data: &Value) -> Result<()> {
    shared_memory::publish(
        &app.db,
        user,
        text(data, "pair", 36)?,
        text(data, "body", 1200)?,
    )
    .await
}

pub(super) async fn remove(app: &App, user: &str, data: &Value) -> Result<()> {
    shared_memory::remove(
        &app.db,
        user,
        text(data, "pair", 36)?,
        text(data, "id", 36)?,
    )
    .await
}

pub(super) async fn open(app: &App, user: &str, data: &Value) -> Result<()> {
    shared_memory::open(&app.db, user, text(data, "pair", 36)?).await
}
