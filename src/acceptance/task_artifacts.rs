use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use tower_service::Service;

#[tokio::test]
async fn retained_uploads_and_versioned_results_download_without_reexecution() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(WidgetChief)));
    let upload = store::attachments::seal(
        &app.vault,
        "1",
        25,
        "source.txt".into(),
        "text/plain",
        b"original upload",
    )
    .unwrap();
    store::ingest_telegram_attachment(&db, "1", 1, "1", "Use this attachment", 25, None, upload)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    for body in ["first retained version", "second retained version"] {
        sqlx::query("INSERT INTO files(task,name,body) VALUES(?,'report.txt',?) ON CONFLICT(task,name) DO UPDATE SET body=excluded.body")
            .bind(&task.id).bind(body).execute(&db).await.unwrap();
        assert!(
            store::outputs::queue_document(&db, &app.vault, &task, "report.txt")
                .await
                .unwrap()
        );
        let sent = store::claim_outbox(&db).await.unwrap().unwrap();
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(sent.id)
            .execute(&db)
            .await
            .unwrap();
    }
    store::settle(&db, &task, "idle", None).await.unwrap();
    assert!(
        store::task_views::complete(&db, "1", &task.id, task.revision)
            .await
            .unwrap()
    );
    let sealed: Vec<String> = sqlx::query_scalar("SELECT ciphertext FROM task_artifacts")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(sealed.len(), 2);
    assert!(
        sealed
            .iter()
            .all(|value| !value.contains("retained version"))
    );
    let outcomes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM telegram_outputs WHERE ciphertext IS NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(outcomes, 2);

    let mut router = web::router(app.clone());
    let request = |user, action, data| {
        Request::post("/api/mini")
            .header("origin", &app.origin)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"init":signed_mini_init(&app, user),"action":action,"data":data})
                    .to_string(),
            ))
            .unwrap()
    };
    let response = router
        .call(request(1, "task-detail", json!({"task":task.id})))
        .await
        .unwrap();
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
    let shelf = detail["files"].as_array().unwrap();
    assert_eq!(shelf.len(), 3);
    assert_eq!(shelf[0]["version"], 2);
    assert_eq!(shelf[1]["version"], 1);
    assert_eq!(shelf[0]["delivery"], "sent");
    assert_eq!(shelf[2]["kind"], "upload");
    assert!(!detail.to_string().contains("retained version"));

    for (item, expected) in shelf.iter().zip([
        "second retained version",
        "first retained version",
        "original upload",
    ]) {
        let data = json!({"task":task.id,"id":item["id"],"kind":item["kind"]});
        let response = router
            .call(request(1, "download-task-file", data.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let downloaded: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 32_000).await.unwrap()).unwrap();
        assert_eq!(
            STANDARD
                .decode(downloaded["content"].as_str().unwrap())
                .unwrap(),
            expected.as_bytes()
        );
        assert_eq!(
            router
                .call(request(2, "download-task-file", data))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert!(store::claim(&db).await.unwrap().is_none());
    sqlx::query("UPDATE task_artifacts SET expires_at=0 WHERE version=1")
        .execute(&db)
        .await
        .unwrap();
    store::expire(&db).await.unwrap();
    let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM task_artifacts")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(retained, 1);
    assert_eq!(
        router
            .call(request(
                1,
                "download-task-file",
                json!({"task":task.id,"id":shelf[1]["id"],"kind":"file"})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}
