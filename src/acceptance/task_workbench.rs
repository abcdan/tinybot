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

#[tokio::test]
async fn starting_a_new_telegram_task_keeps_the_old_run_and_quoted_routing() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let expected = "First task is still working.";
    store::ingest_telegram(&db, "1", 100, "1", "Duplicate task title", 100)
        .await
        .unwrap();
    let first = store::claim(&db).await.unwrap().unwrap();
    store::append(&db, &first.id, "assistant", expected)
        .await
        .unwrap();
    assert!(
        store::start_action(&db, &first, "in-flight-action")
            .await
            .unwrap()
    );
    store::ingest_telegram(&db, "1", 101, "1", "/new Duplicate task title", 101)
        .await
        .unwrap();

    let mut router = web::router(app.clone());
    let request = |user: u64, action: &str, data: Value| {
        Request::post("/api/mini")
            .header("origin", &app.origin)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"init":signed_mini_init(&app, user),"action":action,"data":data})
                    .to_string(),
            ))
            .unwrap()
    };
    let response = router.call(request(1, "status", json!({}))).await.unwrap();
    let status: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
    let second = status["telegram_task"]["id"].as_str().unwrap();
    assert_ne!(second, first.id);
    assert_eq!(status["active_task"], second);
    let tasks = status["tasks"].as_array().unwrap();
    let first_view = tasks.iter().find(|task| task["id"] == first.id).unwrap();
    assert_eq!(first_view["title"], status["telegram_task"]["title"]);
    assert_eq!(first_view["recent_reply"], expected);
    assert!(first_view["created_at"].is_i64());

    let start = json!({"task":first.id,"revision":first.revision});
    let wrong_task = router
        .call(request(1, "start-new-task", start.clone()))
        .await
        .unwrap();
    assert_eq!(wrong_task.status(), StatusCode::CONFLICT);
    let selected: Option<String> = sqlx::query_scalar("SELECT active_task FROM users WHERE id='1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(selected.as_deref(), Some(second));

    let response = router
        .call(request(1, "select-task", start.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let wrong_user = router
        .call(request(2, "start-new-task", start.clone()))
        .await
        .unwrap();
    assert_eq!(wrong_user.status(), StatusCode::CONFLICT);
    let wrong_revision = router
        .call(request(
            1,
            "start-new-task",
            json!({"task":first.id,"revision":first.revision + 1}),
        ))
        .await
        .unwrap();
    assert_eq!(wrong_revision.status(), StatusCode::CONFLICT);
    let response = router
        .call(request(1, "start-new-task", start.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let repeated = router
        .call(request(1, "start-new-task", start))
        .await
        .unwrap();
    assert_eq!(repeated.status(), StatusCode::CONFLICT);
    let selected: Option<String> = sqlx::query_scalar("SELECT active_task FROM users WHERE id='1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(selected.is_none());
    let still_running = task_for(&db, &first.id).await;
    assert_eq!(still_running.state, "running");
    assert_eq!(still_running.revision, first.revision);
    let pending: String =
        sqlx::query_scalar("SELECT state FROM pending WHERE id='in-flight-action'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(pending, "executing");

    store::ingest_telegram(&db, "1", 102, "1", "This starts a third task.", 102)
        .await
        .unwrap();
    let task_for_message = |id| {
        sqlx::query_scalar::<_, String>("SELECT task FROM events WHERE telegram_message_id=?")
            .bind(id)
            .fetch_one(&db)
    };
    let third = task_for_message(102_i64).await.unwrap();
    assert_ne!(third, first.id);
    assert_ne!(third, second);
    sqlx::query(
        "INSERT INTO outbox(user,task,body,telegram_message_id,state,assistant_event_id)
         SELECT '1',task,body,900,'sent',id FROM events WHERE task=? AND role='assistant'",
    )
    .bind(&first.id)
    .execute(&db)
    .await
    .unwrap();
    store::ingest_telegram_reply(
        &db,
        "1",
        103,
        "1",
        "Continue the original task.",
        103,
        Some(store::ReplyContext {
            message_id: 900,
            excerpt: None,
            from_bot: true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(task_for_message(103_i64).await.unwrap(), first.id);
}
