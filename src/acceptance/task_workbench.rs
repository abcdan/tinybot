use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower_service::Service;

#[tokio::test]
async fn task_detail_shows_only_the_owners_latest_result_and_current_review() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    message(&db, 1, "1", "Review the notes").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let trace = audit::Trace::start(&db, &task).await.unwrap();
    audit::scope(
        trace,
        audit::record(
            "tool",
            "browse",
            "completed",
            json!({"page_bytes":32,"raw":"private-tool-detail"}),
        ),
    )
    .await;
    store::append(&db, &task.id, "tool", "private-tool-detail")
        .await
        .unwrap();
    store::append(&db, &task.id, "assistant", "The source is ready to review.")
        .await
        .unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({
            "operation":"Write note", "destination":"https://notes.example",
            "call":{"name":"call_mcp","args":{"arguments":{"token":"private-tool-detail"}}},
            "explanation":"Save this research to the notes service."
        }),
        "Review this note update.",
        None,
        600,
        "fixture-approval",
    )
    .await
    .unwrap();

    let mut router = web::router(app.clone());
    let request = |user, action, data| {
        Request::post("/api/mini")
            .header("origin", &app.origin)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"init":signed_mini_init(&app, user), "action":action, "data":data})
                    .to_string(),
            ))
            .unwrap()
    };
    let read = json!({"task":task.id});
    let denied = router
        .call(request(2, "task-detail", read.clone()))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let response = router
        .call(request(1, "task-detail", read.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
    assert_eq!(detail["task"]["id"], task.id);
    assert_eq!(detail["task"]["revision"], task.revision);
    assert_eq!(detail["task"]["state"], "waiting");
    assert_eq!(detail["reply"]["text"], "The source is ready to review.");
    assert!(detail["audit"]["id"].is_i64());
    assert_eq!(
        detail["last_confirmed"],
        "I had already read a web page for this task."
    );
    assert_eq!(detail["requests"][0]["task"], task.id);
    assert_eq!(detail["requests"][0]["revision"], task.revision);
    assert_eq!(
        detail["requests"][0]["content"]["explanation"],
        "Save this research to the notes service."
    );
    assert!(!detail.to_string().contains("private-tool-detail"));

    let response = router.call(request(1, "status", json!({}))).await.unwrap();
    let home: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
    assert_eq!(detail["requests"], home["requests"]);

    message(&db, 2, "1", "Pause that review").await;
    let stale = router
        .call(request(
            1,
            "select-task",
            json!({"task":task.id,"revision":task.revision}),
        ))
        .await
        .unwrap();
    assert!(stale.status().is_client_error());
    let response = router.call(request(1, "task-detail", read)).await.unwrap();
    let updated: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
    assert_eq!(updated["requests"], json!([]));
}
