use crate::{
    App,
    schedules::webhooks::{self, Delivery},
    security::now,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::json;
use std::sync::Arc;

pub(super) async fn receive(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let header = |key| {
        headers
            .get(key)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    let (status, message) = match webhooks::receive(
        &app.db,
        &app.vault,
        &id,
        header("x-tinybot-timestamp"),
        header("x-tinybot-signature"),
        &body,
        now(),
    )
    .await
    {
        Ok(Delivery::Accepted) => (StatusCode::ACCEPTED, "accepted"),
        Ok(Delivery::Unauthorized) => (StatusCode::UNAUTHORIZED, "webhook unavailable"),
        Ok(Delivery::Busy) => (StatusCode::CONFLICT, "a routine run is already in progress"),
        Ok(Delivery::Invalid) => (StatusCode::BAD_REQUEST, "invalid event"),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "routine temporarily unavailable",
        ),
    };
    (status, Json(json!({"status":message})))
}
