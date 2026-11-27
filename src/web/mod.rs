mod accounts;
mod agents;
mod mini;
mod readiness;
mod routine_webhook;
mod state;

use crate::App;
use accounts::import_codex;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
pub(crate) use mini::chief_operation;
use mini::mini;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(mini_page))
        .route("/health", get(health))
        .route("/ready", get(readiness::ready))
        .route("/mini", get(mini_page))
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("../../web/app.css")) }),
        )
        .route(
            "/mini.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini.js"),
                )
            }),
        )
        .route(
            "/mini-ui.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-ui.js"),
                )
            }),
        )
        .route(
            "/mini-tasks.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-tasks.js"),
                )
            }),
        )
        .route(
            "/mini-artifacts.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-artifacts.js"),
                )
            }),
        )
        .route(
            "/mini-docblock-save.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-docblock-save.js"),
                )
            }),
        )
        .route(
            "/mini-onboarding.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-onboarding.js"),
                )
            }),
        )
        .route(
            "/mini-mail.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-mail.js"),
                )
            }),
        )
        .route(
            "/mini-agents.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-agents.js"),
                )
            }),
        )
        .route(
            "/mini-connectors.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-connectors.js"),
                )
            }),
        )
        .route(
            "/mini-settings-history.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-settings-history.js"),
                )
            }),
        )
        .route(
            "/mini-settings.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-settings.js"),
                )
            }),
        )
        .route(
            "/mini-audit.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-audit.js"),
                )
            }),
        )
        .route(
            "/mini-memory.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-memory.js"),
                )
            }),
        )
        .route(
            "/mini-projects.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-projects.js"),
                )
            }),
        )
        .route(
            "/mini-invites.js",
            get(|| async {
                asset(
                    "application/javascript; charset=utf-8",
                    include_str!("../../web/mini-invites.js"),
                )
            }),
        )
        .route("/api/mini", post(mini))
        .route("/hooks/routine/{id}", post(routine_webhook::receive))
        .route("/api/codex/import", post(import_codex))
        .route("/oauth/mcp", get(oauth))
        .route("/oauth/mcp-client.json", get(mcp_client_metadata))
        .layer(DefaultBodyLimit::max(96_000))
        .layer(middleware::from_fn(headers))
        .with_state(app)
}

async fn mini_page() -> Html<&'static str> {
    Html(include_str!("../../web/mini.html"))
}

fn asset(kind: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, kind)], body)
}

async fn headers(request: axum::extract::Request, next: Next) -> Response {
    let is_mini = matches!(request.uri().path(), "/" | "/mini");
    let mut response = next.run(request).await;
    let csp = if is_mini {
        "default-src 'none'; script-src 'self' https://telegram.org; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'self' https://web.telegram.org"
    } else {
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    };
    let headers = response.headers_mut();
    for (key, value) in [
        ("content-security-policy", csp),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("strict-transport-security", "max-age=31536000"),
    ] {
        headers.insert(key, HeaderValue::from_static(value));
    }
    response
}

async fn health(State(app): State<Arc<App>>) -> impl IntoResponse {
    if sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&app.db)
        .await
        .is_ok()
    {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    }
}

type Api = std::result::Result<Json<Value>, ApiError>;
struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}

fn invalid(_: anyhow::Error) -> ApiError {
    ApiError(
        StatusCode::BAD_REQUEST,
        "TinyBot couldn’t complete this request. Check the settings and try again.",
    )
}
fn forbidden() -> ApiError {
    ApiError(
        StatusCode::FORBIDDEN,
        "This request is expired or unavailable to your account.",
    )
}
fn origin(app: &App, headers: &HeaderMap) -> std::result::Result<(), ApiError> {
    if headers
        .get(header::ORIGIN)
        .is_some_and(|value| value.as_bytes() == app.origin.as_bytes())
    {
        Ok(())
    } else {
        Err(forbidden())
    }
}

async fn oauth(
    State(app): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> Html<&'static str> {
    let state = query.get("state").filter(|state| state.len() == 43);
    let code = query.get("code").filter(|code| code.len() < 8192);
    let issuer = query.get("iss").filter(|issuer| issuer.len() <= 2048);
    match state {
        Some(state)
            if app
                .mcp
                .oauth_finish(state, code.map(String::as_str), issuer.map(String::as_str))
                .await
                .is_ok()
                && code.is_some() =>
        {
            Html(
                "<!doctype html><meta charset=utf-8><title>TinyBot</title><p>Connector verified. You can return to Telegram.</p>",
            )
        }
        _ => Html(
            "<!doctype html><meta charset=utf-8><title>TinyBot</title><p>Connection wasn’t completed. Return to TinyBot in Telegram to retry or enter an API token.</p>",
        ),
    }
}

async fn mcp_client_metadata(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "client_id":format!("{}/oauth/mcp-client.json",app.origin),
        "client_name":"TinyBot",
        "redirect_uris":[format!("{}/oauth/mcp",app.origin)],
        "token_endpoint_auth_method":"none",
        "grant_types":["authorization_code","refresh_token"],
        "response_types":["code"]
    }))
}
