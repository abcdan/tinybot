use super::*;

#[tokio::test]
async fn telegram_status_opens_the_exact_task_and_stale_continue_is_rejected() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, "42").await.unwrap();
    sqlx::query("UPDATE users SET provider='codex' WHERE id='42'")
        .execute(&db)
        .await
        .unwrap();
    store::ingest_telegram(&db, "42", 1, "42", "Write a note.", 20)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    store::pause(
        &db,
        &task,
        "approve",
        &json!({}),
        "Review the note.",
        None,
        600,
        "note-review",
    )
    .await
    .unwrap();
    assert_eq!(
        store::claim_outbox(&db).await.unwrap().unwrap().body,
        "Review the note."
    );
    store::ingest_telegram(&db, "42", 2, "42", "/status", 21)
        .await
        .unwrap();
    let status = store::claim_outbox(&db).await.unwrap().unwrap();

    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([json!({"ok":true,"result":{"message_id":70}})]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let router = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    assert_eq!(
        send_outbound(&db, &telegram, &status).await.unwrap(),
        MessageDelivery::Sent(Some(70))
    );
    let data = {
        let captured = mock.lock().await;
        let keyboard = &captured.0[0].1["reply_markup"]["inline_keyboard"];
        assert_eq!(keyboard[1][0]["text"], "Review");
        assert_eq!(
            keyboard[1][0]["web_app"]["url"],
            format!("https://example.test/mini?task={}", task.id)
        );
        assert_eq!(
            keyboard[0][0]["callback_data"],
            format!("tc/{}/{}", task.id, task.revision)
        );
        keyboard[0][0]["callback_data"].clone()
    };
    store::ingest_telegram(&db, "42", 3, "42", "Update the plan.", 22)
        .await
        .unwrap();
    let click = serde_json::from_value(json!({"id":"fixture","from":{"id":42,"is_bot":false},
        "message":{"message_id":70,"chat":{"id":42,"type":"private"}},"data":data}))
    .unwrap();
    assert_eq!(
        callback::answer(&db, &click).await.unwrap().text,
        "This option is no longer available."
    );
    server.abort();
}
