use super::text;
use crate::{
    App, audit,
    store::{invitations, shared_memory},
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub(super) async fn list(app: &App, user: &str) -> Result<Value> {
    let (pending, guests) = tokio::try_join!(
        invitations::pending(&app.db, user),
        invitations::guests(&app.db, user)
    )?;
    Ok(json!({"pending":pending,"guests":guests}))
}

pub(super) async fn my_usage(app: &App, user: &str) -> Result<Value> {
    let mut tx = app.db.begin().await?;
    let (accepted, version, model_connected, first_result): (
        Option<i64>,
        Option<i64>,
        bool,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT i.accepted_at,i.insights_version,u.provider IS NOT NULL,u.first_result_at_ms
             FROM users u LEFT JOIN bot_invitations i ON i.member=u.id
             WHERE u.id=? AND u.suspended_at IS NULL",
    )
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    let mut summary = audit::account_usage(&mut tx, user).await?;
    summary["guest"] = if let Some(accepted) = accepted {
        json!({"accepted_at":accepted,"insights_version":version,"model_connected":model_connected,
            "first_result_at_ms":first_result})
    } else {
        Value::Null
    };
    summary["alert"] = json!(audit::alert_usage(&mut tx, user).await?);
    tx.commit().await?;
    Ok(summary)
}

pub(super) async fn save_usage_alert(app: &App, user: &str, data: &Value) -> Result<()> {
    let amount = data
        .get("weekly_cents")
        .context("weekly alert amount required")?;
    let cents = (!amount.is_null())
        .then(|| amount.as_i64().context("invalid weekly alert amount"))
        .transpose()?;
    audit::set_alert(&app.db, user, cents).await
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
