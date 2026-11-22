use super::{Api, forbidden, invalid};
use crate::{
    App,
    security::{hash, now},
    siwc::Profile,
};
use anyhow::Result;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, header},
};
use serde_json::json;
use std::sync::Arc;

pub(super) async fn configure(
    app: &App,
    user: &str,
    kind: &str,
    model: &str,
    credential: &str,
) -> Result<()> {
    let mut tx = app.db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE users SET provider=?,model=?,credential=? WHERE id=?")
        .bind(kind)
        .bind(model)
        .bind(credential)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE tasks SET state='queued' WHERE user=? AND state='setup'")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn import_codex(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(mut profile): Json<Profile>,
) -> Api {
    let pairing = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|code| code.len() == 43)
        .ok_or_else(forbidden)?;
    let pair: Option<(String, String)> = sqlx::query_as(
        "DELETE FROM pending WHERE id=? AND kind='codex-pair' AND expires>=?
        RETURNING user,payload",
    )
    .bind(hash(pairing))
    .bind(now())
    .fetch_optional(&app.db)
    .await
    .map_err(|e| invalid(e.into()))?;
    let (user, model) = pair.ok_or_else(forbidden)?;
    profile.validate_import().await.map_err(invalid)?;
    let bytes = serde_json::to_vec(&profile).map_err(|e| invalid(e.into()))?;
    let encrypted = app
        .vault
        .seal(&format!("model:{user}"), &bytes)
        .map_err(invalid)?;
    configure(&app, &user, "codex", &model, &encrypted)
        .await
        .map_err(invalid)?;
    Ok(Json(json!({"ok":true})))
}
