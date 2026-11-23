use super::{WidgetChief, app_with_model, fixture, signed_mini_init};
use crate::web;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn only_the_signed_operator_can_change_browser_proxy_settings() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let operator = signed_mini_init(&app, 1);
    let guest = signed_mini_init(&app, 2);
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

    let state: Value = post(&guest, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(state["browser_proxy"].is_null());
    for action in [
        "save-browser-proxy",
        "remove-browser-proxy",
        "disable-browser-proxy",
    ] {
        assert_eq!(
            post(
                &guest,
                action,
                json!({"url":"http://relay:synthetic@proxy.example:3128"})
            )
            .await
            .unwrap()
            .status(),
            403
        );
    }
    let status: Value = post(&operator, "status", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let browser = status["browser_proxy"].as_object().unwrap();
    assert!(browser.contains_key("source"));
    assert!(browser.contains_key("overridden"));
    assert_eq!(browser.len(), 2);
    let rejected = post(
        &operator,
        "save-browser-proxy",
        json!({"url":"socks5://relay:synthetic@127.0.0.1:1080"}),
    )
    .await
    .unwrap();
    assert_eq!(rejected.status(), 400);
    assert!(!rejected.text().await.unwrap().contains("synthetic"));
    server.abort();
}
