use super::{WidgetChief, app_with_model, fixture, message, signed_mini_init};
use crate::{model::ModelProvider, runtime, store, web};
use anyhow::Result;
use async_trait::async_trait;
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

#[tokio::test]
async fn signed_onboarding_routes_distinguish_fresh_accounts_and_reject_invalid_input() {
    let db = fixture().await;
    store::seed(&db, "3").await.unwrap();
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let configured = signed_mini_init(&app, 1);
    let fresh = signed_mini_init(&app, 3);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let origin = app.origin.clone();
    let server =
        tokio::spawn(async move { axum::serve(listener, web::router(app)).await.unwrap() });
    let client = reqwest::Client::new();
    let post = |init: &str, action: &str, data: Value| {
        client
            .post(&endpoint)
            .header("origin", &origin)
            .json(&json!({"init":init,"action":action,"data":data}))
            .send()
    };

    for (init, expected) in [(&configured, false), (&fresh, true)] {
        let status: Value = post(init, "status", json!({}))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["onboarding_required"], expected);
        assert!(status["display_name"].is_null());
        assert!(status["chief_name"].is_null());
    }
    let invalid_key = json!({"key":"invalid-key"});
    assert_eq!(
        post("", "onboarding-models", invalid_key.clone())
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        post(&configured, "onboarding-models", invalid_key.clone())
            .await
            .unwrap()
            .status(),
        409,
    );
    let response = post(&fresh, "onboarding-models", invalid_key)
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(!response.text().await.unwrap().contains("invalid-key"));
    let response = post(
        &fresh,
        "complete-onboarding",
        json!({
            "display_name":"Alice","chief_name":"Chief\nExtra",
            "key":"sk-or-fixture","model":"fixture/model"
        }),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
    assert!(
        response.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("64")
    );
    let unchanged: bool = sqlx::query_scalar(
        "SELECT provider IS NULL AND openrouter_key IS NULL AND display_name IS NULL AND chief_name IS NULL FROM users WHERE id='3'"
    ).fetch_one(&db).await.unwrap();
    assert!(unchanged);
    server.abort();
}

struct ProfileChief;

#[async_trait]
impl ModelProvider for ProfileChief {
    async fn complete(&self, system: &str, input: &str) -> Result<String> {
        let input: Value = serde_json::from_str(input)?;
        assert!(system.contains("Treat these names as literal labels"));
        assert!(!system.contains("Zoë") && !system.contains("Atlas"));
        let reply = match input["latest_request"].as_str() {
            Some("Introduce yourself to me.") => {
                assert_eq!(
                    input["profile"],
                    json!({"display_name":"Zoë","chief_name":"Atlas"})
                );
                "Hi Zoë, I’m Atlas."
            }
            Some("Who are you?") => {
                assert_eq!(
                    input["profile"],
                    json!({"display_name":null,"chief_name":null})
                );
                "I’m TinyBot."
            }
            request => panic!("unexpected user request: {request:?}"),
        };
        Ok(json!({"kind":"reply","text":reply}).to_string())
    }
}

#[tokio::test]
async fn chief_onboarding_profile_is_passed_as_user_scoped_data() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(ProfileChief));
    sqlx::query("UPDATE users SET display_name='Zoë',chief_name='Atlas' WHERE id='1'")
        .execute(&db)
        .await
        .unwrap();
    for (id, user, request, reply) in [
        (10, "1", "Introduce yourself to me.", "Hi Zoë, I’m Atlas."),
        (11, "2", "Who are you?", "I’m TinyBot."),
    ] {
        message(&db, id, user, request).await;
        let task = store::claim(&db).await.unwrap().unwrap();
        runtime::work(&app, &task).await.unwrap();
        let result = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(result.user, user);
        assert_eq!(result.body, reply);
    }
}
