use super::{WidgetChief, app_with_model, fixture, signed_mini_init};
use crate::web;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn the_signed_model_settings_keep_the_shared_key_when_stopping_the_task_model() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let credential = app.vault.seal("openrouter:1", b"sk-or-fixture").unwrap();
    sqlx::query("UPDATE users SET openrouter_key=? WHERE id='1'")
        .bind(&credential)
        .execute(&db)
        .await
        .unwrap();
    let owner = signed_mini_init(&app, 1);
    let other = signed_mini_init(&app, 2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let origin = app.origin.clone();
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::new();
    let post = |init: &str, action: &str| {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":init,"action":action}))
            .send()
    };

    let public_state: Value = post(&owner, "status").await.unwrap().json().await.unwrap();
    assert_eq!(public_state["openrouter_connected"], true);
    assert!(!public_state.to_string().contains(&credential));
    let missing = post(&other, "openrouter-models").await.unwrap();
    assert_eq!(missing.status(), 400);
    assert_eq!(
        missing.json::<Value>().await.unwrap()["error"],
        "Connect your OpenRouter key first."
    );
    assert!(
        post(&owner, "disconnect-model")
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let state: Value = post(&owner, "status").await.unwrap().json().await.unwrap();
    assert!(state["provider"].is_null());
    assert_eq!(state["openrouter_connected"], true);
    assert!(
        post(&owner, "disconnect-openrouter")
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let state: Value = post(&owner, "status").await.unwrap().json().await.unwrap();
    assert_eq!(state["openrouter_connected"], false);
    server.abort();
}
