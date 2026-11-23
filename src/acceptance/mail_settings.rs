use super::{WidgetChief, app_with_model, fixture, signed_mini_init};
use crate::web;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn signed_email_settings_keep_credentials_private_and_scope_accounts_to_the_owner() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let owner = signed_mini_init(&app, 1);
    let other = signed_mini_init(&app, 2);
    let origin = app.origin.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
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
    let config = json!({"label":"Work","address":"fixture@example.com", "imap":{
        "host":"imap.example.com","port":993,"username":"fixture@example.com","password":"test-imap-secret"
    }, "smtp":{
        "host":"smtp.example.com","port":587,"username":"fixture@example.com","password":"test-smtp-secret"
    }});
    let response = post("", "save-mail-account", json!({"config":config}))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = post(&owner, "save-mail-account", json!({"config":config}))
        .await
        .unwrap();
    assert!(response.status().is_success());
    let saved: Value = response.json().await.unwrap();
    let id = saved["id"].as_str().unwrap();

    let state: Value = post(&owner, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!state.to_string().contains("test-imap-secret"));
    assert!(!state.to_string().contains("test-smtp-secret"));
    let settings = &state["mail_accounts"][0];
    assert_eq!(settings["id"], id);
    assert_eq!(
        settings["imap"],
        json!({"host":"imap.example.com","port":993,"username":"fixture@example.com"})
    );
    assert_eq!(
        settings["smtp"],
        json!({"host":"smtp.example.com","port":587,"username":"fixture@example.com"})
    );
    assert_eq!(
        settings["folders"],
        json!([{"name":"INBOX","automate":false,"instruction":null}])
    );

    assert_eq!(
        post(
            &other,
            "save-mail-account",
            json!({"id":id,"config":config})
        )
        .await
        .unwrap()
        .status(),
        400
    );
    assert_eq!(
        post(&other, "remove-mail-account", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );
    let other_state: Value = post(&other, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(other_state["mail_accounts"], json!([]));
    assert!(
        post(&owner, "remove-mail-account", json!({"id":id}))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let owner_state: Value = post(&owner, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(owner_state["mail_accounts"], json!([]));
    server.abort();
}
