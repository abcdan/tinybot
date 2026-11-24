use super::{Api, ApiError, agents, forbidden, invalid, origin, state};
use crate::{
    App,
    model::{OnboardingError, OpenRouterSettingsError},
    security::telegram_user,
};
use anyhow::{Context, Result, bail};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

mod browser;
mod connectors;
mod mail;
mod memory;
mod models;
mod onboarding;
mod sharing;
mod tasks;

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
    operation(&app, &user, action, &request.data)
        .await
        .map(Json)
        .map_err(|error| {
            if action == "get-form" {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "This form is no longer available. Open a fresh form from TinyBot in Telegram.",
                )
            } else if matches!(action, "audit-session" | "audit-export") {
                ApiError(StatusCode::NOT_FOUND, "This audit session is no longer available.")
            } else if let Some(error) = error.downcast_ref::<browser::SettingsError>() {
                error.response()
            } else if let Some(error) = error.downcast_ref::<OnboardingError>() {
                let status = match error {
                    OnboardingError::InvalidName => StatusCode::BAD_REQUEST,
                    OnboardingError::AlreadyConfigured => StatusCode::CONFLICT,
                };
                ApiError(status, error.message())
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

async fn operation(app: &App, user: &str, action: &str, data: &Value) -> Result<Value> {
    match action {
        "status" => return state::status(app, user).await,
        "onboarding-models" => return onboarding::models(app, user, data).await,
        "complete-onboarding" => onboarding::complete(app, user, data).await?,
        "audit-sessions" => return tasks::audit_sessions(app, user, data).await,
        "audit-session" => return tasks::audit_session(app, user, data).await,
        "audit-export" => return tasks::audit_export(app, user, data).await,
        "save-browser-proxy" => browser::save(app, user, data)?,
        "remove-browser-proxy" => browser::reset(app, user)?,
        "disable-browser-proxy" => browser::direct(app, user)?,
        "save-mail-account" => return mail::save(app, user, data).await,
        "remove-mail-account" => mail::remove(app, user, data).await?,

        "openrouter-key" => return models::save_openrouter_key(app, user, data).await,
        "openrouter-models" => return models::openrouter_models(app, user).await,
        "openrouter-model" => models::select_openrouter_model(app, user, data).await?,
        "openrouter-server-tool" => {
            app.models
                .set_server_tool(
                    user,
                    text(data, "tool", 32)?,
                    data["enabled"]
                        .as_bool()
                        .context("invalid enabled setting")?,
                )
                .await?
        }
        "disconnect-openrouter" => app.models.disconnect_openrouter(user).await?,
        "codex-pair" => return models::pair_codex(app, user, data).await,
        "codex-select" => app.models.select_codex(user).await?,
        "disconnect-codex" => app.models.disconnect_codex(user).await?,
        "disconnect-model" => app.models.disconnect_model(user).await?,

        "save-agent" => agents::save_agent(app, user, data).await?,
        "invite-agent" => return sharing::invite_agent(app, user, data).await,
        "revoke-member" => sharing::revoke_member(app, user, data).await?,
        "publish-memory" => sharing::publish_memory(app, user, data).await?,
        "list-memories" => return memory::list(app, user, data).await,
        "save-memory" => memory::save(app, user, data).await?,
        "delete-memory" => memory::remove(app, user, data).await?,

        "grant-connector" => connectors::grant(app, user, data).await?,
        "revoke-connector" => connectors::revoke(app, user, data).await?,
        "search-mcp" => return connectors::search_mcp(app, data).await,
        "add-mcp" => return connectors::add_mcp(app, user, data).await,
        "add-anything-mcp" => return connectors::add_anything_mcp(app, user, data).await,
        "mcp-tools" => return connectors::list_tools(app, user, data).await,
        "mcp-oauth" => return connectors::oauth_start(app, user, data).await,
        "mcp-token" => connectors::save_token(app, user, data).await?,
        "mcp-public" => connectors::save_public(app, user, data).await?,
        "disconnect-mcp" => connectors::disconnect(app, user, data).await?,
        "revoke-mcp-approval" => connectors::revoke_approval(app, user, data).await?,

        "select-agent" => bail!("Message your Chief in Telegram. It can delegate to saved agents."),
        "select-task" => tasks::select_task(app, user, data).await?,
        "get-form" => return tasks::get_form(app, user, data).await,
        "submit-form" => tasks::submit_form(app, user, data).await?,
        "answer-form" => tasks::answer_form(app, user, data).await?,
        "time-zone" => tasks::set_time_zone(app, user, data).await?,
        "cancel-schedule" => tasks::cancel_schedule(app, user, data).await?,
        "approve" | "deny" => tasks::respond(app, user, data, action == "approve").await?,
        "always-approve-mcp" => tasks::always_approve_mcp(app, user, data).await?,
        _ => bail!("unknown Mini App action"),
    }
    Ok(json!({"ok":true}))
}
