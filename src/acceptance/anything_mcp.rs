use super::{WidgetChief, app_with_model, fixture, signed_mini_init};
use crate::web;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn signed_anything_mcp_setup_records_only_the_users_self_hosted_endpoint() {
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
    let data = json!({"endpoint":"https://gateway.example.com/mcp/work"});
    assert_eq!(
        post("", "add-anything-mcp", data.clone())
            .await
            .unwrap()
            .status(),
        403
    );
    for invalid in [
        "http://gateway.example.com/mcp/work",
        "https://admin:password@gateway.example.com/mcp/work",
        "https://gateway.example.com/mcp/work?token=secret",
        "https://cloud.anythingmcp.com/mcp/work",
        "https://anythingmcp.com./mcp",
        "stdio://work",
    ] {
        let response = post(&owner, "add-anything-mcp", json!({"endpoint":invalid}))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }

    let connection = post(&owner, "add-anything-mcp", data.clone())
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let id = connection["id"].as_str().unwrap();
    let retry = post(
        &owner,
        "add-anything-mcp",
        json!({"endpoint":"https://gateway.example.com/mcp/work/"}),
    )
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(retry["id"], id);

    let details = post(&owner, "status", json!({}))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let stored = details["connectors"].as_array().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["endpoint"], data["endpoint"]);
    assert_eq!(stored[0]["kind"], "anythingmcp");
    assert_eq!(stored[0]["auth"], "pending");
    assert!(stored[0].get("credential").is_none());

    assert_eq!(
        post(&owner, "mcp-public", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );

    assert_eq!(
        post(&owner, "mcp-tools", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        post(&other, "mcp-tools", json!({"id":id}))
            .await
            .unwrap()
            .status(),
        400
    );
    let other_details = post(&other, "status", json!({}))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other_details["connectors"], json!([]));
    server.abort();
}
