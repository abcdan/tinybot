use super::{Api, forbidden, invalid};
use crate::{
    App,
    security::{hash, now},
    siwc::Profile,
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, header},
};
use serde_json::json;
use std::sync::Arc;

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
        "DELETE FROM pending WHERE id=? AND kind='codex-pair' AND state='waiting' AND expires>=?
        AND EXISTS(SELECT 1 FROM users WHERE id=pending.user AND suspended_at IS NULL)
        RETURNING user,payload",
    )
    .bind(hash(pairing))
    .bind(now())
    .fetch_optional(&app.db)
    .await
    .map_err(|e| invalid(e.into()))?;
    let (user, model) = pair.ok_or_else(forbidden)?;
    profile.validate_import().await.map_err(invalid)?;
    app.models
        .connect_codex(&user, &model, &profile)
        .await
        .map_err(invalid)?;
    Ok(Json(json!({"ok":true})))
}
