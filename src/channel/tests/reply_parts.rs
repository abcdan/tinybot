use super::*;

#[tokio::test]
async fn long_telegram_replies_deliver_in_order_and_share_one_transcript_event() {
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
    store::ingest_telegram_reply(
        &db,
        "42",
        1,
        "42",
        "Write the complete result.",
        20,
        Some(store::ReplyContext {
            message_id: 19,
            excerpt: None,
            from_bot: false,
        }),
    )
    .await
    .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    let answer = format!("{}Final sentence.", "é😀".repeat(3000));
    store::settle_reply(&db, &task, &answer, &["Summarize"])
        .await
        .unwrap();

    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}}),
            json!({"ok":true,"result":{"message_id":71}}),
            json!({"ok":true,"result":{"message_id":72}}),
            json!({"ok":true,"result":{"message_id":73}}),
        ]),
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

    let first = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    assert_eq!(
        send_outbound(&db, &telegram, &first).await.unwrap(),
        MessageDelivery::Retry(3)
    );
    sqlx::query("UPDATE outbox SET state='queued',retry_at=? WHERE id=?")
        .bind(crate::security::now() + 60)
        .bind(first.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    sqlx::query("UPDATE outbox SET retry_at=0 WHERE id=?")
        .bind(first.id)
        .execute(&db)
        .await
        .unwrap();

    for message in 71..=73 {
        let part = store::claim_outbox(&db).await.unwrap().unwrap();
        if message == 71 {
            assert_eq!(part.id, first.id);
        }
        assert_eq!(
            send_outbound(&db, &telegram, &part).await.unwrap(),
            MessageDelivery::Sent(Some(message))
        );
        sqlx::query("UPDATE outbox SET state='sent',telegram_message_id=? WHERE id=?")
            .bind(message)
            .bind(part.id)
            .execute(&db)
            .await
            .unwrap();
    }
    assert!(store::claim_outbox(&db).await.unwrap().is_none());

    let captured = mock.lock().await;
    assert_eq!(captured.0.len(), 4);
    assert_eq!(captured.0[0], captured.0[1]);
    let delivered = &captured.0[1..];
    let text = delivered
        .iter()
        .map(|(method, payload)| {
            assert_eq!(method, "sendMessage");
            assert_eq!(payload["reply_parameters"]["message_id"], 20);
            let text = payload["text"].as_str().unwrap();
            assert!(text.encode_utf16().count() <= 4096);
            text
        })
        .collect::<String>();
    assert_eq!(text, answer);
    assert!(
        delivered[..2]
            .iter()
            .all(|(_, payload)| payload.get("reply_markup").is_none())
    );
    assert_eq!(
        delivered[2].1["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Summarize"
    );
    drop(captured);

    let history = store::history(&db, &task.id).await.unwrap();
    let replies: Vec<_> = history
        .iter()
        .filter(|(_, role, _)| role == "assistant")
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].2, answer);

    store::ingest_telegram(&db, "42", 2, "42", "/new Different topic.", 21)
        .await
        .unwrap();
    let other = store::claim(&db).await.unwrap().unwrap();
    assert_ne!(other.id, task.id);
    store::settle(&db, &other, "idle", None).await.unwrap();
    store::ingest_telegram_reply(
        &db,
        "42",
        3,
        "42",
        "Look at that part again.",
        22,
        Some(store::ReplyContext {
            message_id: 72,
            excerpt: None,
            from_bot: true,
        }),
    )
    .await
    .unwrap();
    assert_eq!(store::claim(&db).await.unwrap().unwrap().id, task.id);
    server.abort();
}
