use super::{Api, ApiError, agents, forbidden, invalid, origin, state};
use crate::{
    App, audit,
    mcp::Candidate,
    model::OpenRouterSettingsError,
    security::{hash, id, now, telegram_user, token},
    store,
};
use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Deserialize)]
pub(super) struct Mini {
    init: String,
    action: String,
    #[serde(default)]
    data: Value,
}

pub(super) async fn mini(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(request): Json<Mini>,
) -> Api {
    origin(&app, &headers)?;
    let user = telegram_user(&request.init, &app.token).map_err(|_| forbidden())?;
    let onboarded = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE id=?)")
        .bind(&user)
        .fetch_one(&app.db)
        .await
        .map_err(|e| invalid(e.into()))?;
    if !onboarded {
        return Err(forbidden());
    }
    let action = request.action.as_str();
    operation(&app, &user, action, request.data)
        .await
        .map(Json)
        .map_err(|error| {
            if action == "get-form" {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "This form is no longer available. Open a fresh form from TinyBot in Telegram.",
                )
            } else if action == "audit-session" {
                ApiError(StatusCode::NOT_FOUND, "This audit session is no longer available.")
            } else if let Some(error) = error.downcast_ref::<OpenRouterSettingsError>() {
                match error {
                    OpenRouterSettingsError::MissingKey => ApiError(StatusCode::BAD_REQUEST, "Connect your OpenRouter key first."),
                    OpenRouterSettingsError::InvalidKey => ApiError(StatusCode::BAD_REQUEST, "OpenRouter rejected that key. Enter an active OpenRouter API key."),
                    OpenRouterSettingsError::InvalidModel => ApiError(StatusCode::BAD_REQUEST, "That model is no longer available for tool calls. Refresh the model list and choose another."),
                    OpenRouterSettingsError::Unavailable => ApiError(StatusCode::BAD_GATEWAY, "TinyBot couldn’t reach OpenRouter. Try again in a moment."),
                    OpenRouterSettingsError::Changed => ApiError(StatusCode::CONFLICT, "Your OpenRouter key changed. Reload the model list and try again."),
                }
            } else {
                invalid(error)
            }
        })
}

pub(super) fn text<'a>(data: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    data[key]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= max)
        .with_context(|| format!("invalid {key}"))
}

async fn operation(app: &App, user: &str, action: &str, data: Value) -> Result<Value> {
    let mut result = json!({"ok":true});
    match action {
        "status" => return state::status(app, user).await,
        "audit-sessions" => {
            return audit::sessions(&app.db, user, audit_cursor(&data, "before")?).await;
        }
        "audit-session" => {
            let session = audit_cursor(&data, "id")?.context("audit session ID required")?;
            return audit::session(
                &app.db,
                user,
                session,
                audit_cursor(&data, "after")?.unwrap_or(0),
            )
            .await;
        }
        "get-form" => {
            let request = text(&data, "id", 36)?;
            return crate::forms::input_request(&app.db, user, request).await;
        }
        "openrouter-key" => {
            let models = app
                .models
                .save_openrouter_key(user, text(&data, "key", 8192)?)
                .await?;
            result = json!({"models":models});
        }
        "openrouter-models" => {
            result = json!({"models":app.models.openrouter_models(user).await?});
        }
        "openrouter-model" => {
            app.models
                .select_openrouter_model(user, text(&data, "model", 150)?)
                .await?;
        }
        "disconnect-openrouter" => {
            app.models.disconnect_openrouter(user).await?;
        }
        "codex-pair" => {
            let model = text(&data, "model", 150)?;
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
            sqlx::query(
                "INSERT INTO pending(id,user,kind,payload,expires) VALUES(?,?,'codex-pair',?,?)",
            )
            .bind(hash(&pairing))
            .bind(user)
            .bind(model)
            .bind(now() + 900)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            result =
                json!({"code":pairing,"command":format!("tinybot codex connect {}",app.origin)});
        }
        "codex-select" => {
            app.models.select_codex(user).await?;
        }
        "disconnect-codex" => {
            app.models.disconnect_codex(user).await?;
        }
        "disconnect-model" => {
            app.models.disconnect_model(user).await?;
        }
        "save-agent" => agents::save_agent(app, user, &data).await?,
        "invite-agent" => {
            let agent = text(&data, "agent", 64)?;
            let invitation = token();
            let affected=sqlx::query("INSERT INTO invitations(token,issuer,agent,expires) SELECT ?,owner,id,? FROM agents
                WHERE id=? AND owner=?").bind(hash(&invitation)).bind(now()+86400).bind(agent).bind(user)
                .execute(&app.db).await?.rows_affected();
            ensure!(affected == 1, "agent unavailable");
            result = json!({"command":format!("/start {invitation}"),"expires_in":86400});
        }
        "revoke-member" => {
            let agent = text(&data, "agent", 64)?;
            let member = text(&data, "member", 24)?;
            sqlx::query("DELETE FROM grants WHERE agent=? AND member=? AND EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)")
                .bind(agent).bind(member).bind(agent).bind(user).execute(&app.db).await?;
        }
        "publish-memory" => {
            let agent = text(&data, "agent", 64)?;
            let body = text(&data, "body", 1200)?;
            let changed=sqlx::query("INSERT INTO memory(id,owner,agent,body) SELECT ?,owner,id,? FROM agents WHERE id=? AND owner=?")
                .bind(id()).bind(body).bind(agent).bind(user).execute(&app.db).await?.rows_affected();
            ensure!(changed == 1, "agent unavailable");
        }
        "delete-memory" => {
            sqlx::query("DELETE FROM memory WHERE id=? AND owner=?")
                .bind(text(&data, "id", 64)?)
                .bind(user)
                .execute(&app.db)
                .await?;
        }
        "grant-connector" => {
            app.mcp
                .grant(
                    user,
                    text(&data, "agent", 64)?,
                    text(&data, "connector", 64)?,
                    text(&data, "tool", 128)?,
                    &data["scope"],
                )
                .await?
        }
        "revoke-connector" => {
            sqlx::query(
                "DELETE FROM connector_grants WHERE agent=? AND connector=? AND tool=? AND
                EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)",
            )
            .bind(text(&data, "agent", 64)?)
            .bind(text(&data, "connector", 64)?)
            .bind(text(&data, "tool", 128)?)
            .bind(text(&data, "agent", 64)?)
            .bind(user)
            .execute(&app.db)
            .await?;
        }
        "select-agent" => {
            anyhow::bail!("Message your Chief in Telegram. It can delegate to saved agents.");
        }
        "select-task" => {
            let task = text(&data, "task", 64)?;
            ensure!(
                store::select_task(&app.db, user, task).await?,
                "task unavailable"
            );
        }
        "search-mcp" => {
            result = serde_json::to_value(
                app.mcp
                    .discover(&app.workers, text(&data, "name", 2048)?)
                    .await?,
            )?;
        }
        "add-mcp" => {
            text(&data, "name", 100)?;
            text(&data, "endpoint", 2048)?;
            let candidate: Candidate = serde_json::from_value(data.clone())?;
            ensure!(
                data["confirmed"] == true,
                "confirm this connector destination"
            );
            result = json!({"id":app.mcp.prepare(user,&candidate).await?});
        }
        "mcp-oauth" => {
            let client = data["client"]
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 2048);
            let secret = data["secret"]
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 8192);
            result = json!({"consent_url":app.mcp.oauth_start(user,text(&data,"id",64)?,client,secret).await?});
        }
        "mcp-token" => {
            let connector = text(&data, "id", 64)?;
            app.mcp
                .save_api(
                    user,
                    connector,
                    text(&data, "token", 8192)?,
                    data["header"].as_str().unwrap_or("Authorization"),
                    data["prefix"].as_str().unwrap_or("Bearer"),
                )
                .await?;
            app.mcp.wake_connected(user, connector).await?;
        }
        "mcp-public" => app.mcp.save_public(user, text(&data, "id", 64)?).await?,
        "disconnect-mcp" => {
            sqlx::query("DELETE FROM connectors WHERE id=? AND owner=?")
                .bind(text(&data, "id", 64)?)
                .bind(user)
                .execute(&app.db)
                .await?;
        }
        "submit-form" => {
            let request = text(&data, "id", 64)?;
            let values: BTreeMap<String, String> = serde_json::from_value(data["values"].clone())?;
            app.forms.receive(&app.db, user, request, values).await?;
        }
        "answer-form" => {
            let request = text(&data, "id", 64)?;
            let values: BTreeMap<String, String> = serde_json::from_value(data["values"].clone())?;
            crate::forms::answer(&app.db, user, request, values).await?;
        }
        "time-zone" => crate::schedules::set_zone(&app.db, user, text(&data, "zone", 100)?).await?,
        "cancel-schedule" => {
            crate::schedules::cancel(&app.db, user, text(&data, "id", 64)?).await?
        }
        "approve" | "deny" => {
            store::respond(&app.db, user, text(&data, "id", 64)?, action == "approve").await?
        }
        _ => return Err(anyhow!("unknown Mini App action")),
    }
    Ok(result)
}

fn audit_cursor(data: &Value, field: &str) -> Result<Option<i64>> {
    data.get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_i64()
                .filter(|id| *id > 0)
                .context("invalid audit cursor")
        })
        .transpose()
}
