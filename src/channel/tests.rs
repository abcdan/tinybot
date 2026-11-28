use super::{
    dispatch::{handle_callback, send_outbound, show_invitation},
    telegram::{suggested_keyboard, widget_rejection},
    *,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc};
use tokio::{net::TcpListener, sync::Mutex};

type Mock = Arc<Mutex<(Vec<(String, Value)>, VecDeque<Value>)>>;

mod reply_parts;
mod task_status;

#[tokio::test]
async fn settings_form_links_open_the_signed_mini_app_and_expire_on_steering() {
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
    store::ingest_telegram(&db, "42", 1, "42", "Change my time zone.", 20)
        .await
        .unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let task = store::claim(&db).await.unwrap().unwrap();
    let request = crate::security::id();
    let button = format!("{}{request}", store::settings_requests::BUTTON_PREFIX);
    store::pause(
        &db,
        &task,
        "settings-form",
        &json!({}),
        "Review your settings.",
        Some(&button),
        600,
        &request,
    )
    .await
    .unwrap();
    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(store::can_deliver(&db, &item).await.unwrap());

    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}}),
            json!({"ok":true,"result":{"message_id":70}}),
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
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Retry(3)
    );
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Sent(Some(70))
    );
    let expected_url = format!("https://example.test/mini?form={request}");
    let captured = mock.lock().await;
    assert_eq!(captured.0.len(), 2);
    for (method, request) in &captured.0 {
        assert_eq!(method, "sendMessage");
        assert_eq!(request["chat_id"], 42);
        assert_eq!(
            request["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
            expected_url
        );
    }
    store::ingest_telegram(&db, "42", 2, "42", "Keep my current settings.", 21)
        .await
        .unwrap();
    assert!(!store::can_deliver(&db, &item).await.unwrap());
    server.abort();
}

#[tokio::test]
async fn accepted_telegram_input_only_schedules_delayed_progress() {
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
    let arrived_at = crate::security::now();
    for _ in 0..2 {
        store::ingest_telegram(&db, "42", 1, "42", "Draft the plan.", 20)
            .await
            .unwrap();
    }
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    store::claim(&db).await.unwrap().unwrap();
    store::ingest_telegram(&db, "42", 2, "42", "Add milestones.", 21)
        .await
        .unwrap();
    store::ingest_telegram(&db, "42", 3, "42", "/new Queue a second task.", 22)
        .await
        .unwrap();
    let pending: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT reply_to_message_id,retry_at,body FROM outbox
         WHERE state='queued' AND button LIKE 'ack-generate/%' ORDER BY reply_to_message_id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|(message, _, _)| *message)
            .collect::<Vec<_>>(),
        [21, 22]
    );
    assert!(
        pending
            .iter()
            .all(|(_, retry_at, body)| *retry_at >= arrived_at + 6 && body.is_empty())
    );
    sqlx::query("UPDATE outbox SET retry_at=0 WHERE button LIKE 'ack-generate/%'")
        .execute(&db)
        .await
        .unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
}

#[tokio::test]
async fn generated_progress_replies_to_the_request_without_controls() {
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
    store::ingest_telegram(&db, "42", 1, "42", "Zoek de openingstijden op.", 20)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    sqlx::query("UPDATE outbox SET retry_at=0 WHERE button LIKE 'ack-generate/%'")
        .execute(&db)
        .await
        .unwrap();
    let progress = store::acknowledgements::claim(&db).await.unwrap().unwrap();
    let text = "Ik zoek de openingstijden op.";
    store::acknowledgements::finish(&db, &progress, Some(text), false)
        .await
        .unwrap();
    let item = store::claim_outbox(&db).await.unwrap().unwrap();

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
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Sent(Some(70))
    );
    store::settle_reply(&db, &task, "De winkel opent om 9.00 uur.", &[])
        .await
        .unwrap();
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Skipped
    );

    let captured = mock.lock().await;
    assert_eq!(captured.0.len(), 1);
    let (method, request) = &captured.0[0];
    assert_eq!(method, "sendMessage");
    assert_eq!(request["text"], text);
    assert_eq!(request["reply_parameters"]["message_id"], 20);
    assert!(request.get("reply_markup").is_none());
    server.abort();
}

#[tokio::test]
async fn telegram_invite_discloses_usage_before_private_account_acceptance() {
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":true,"result":{"message_id":70}}),
            json!({"ok":true,"result":true}),
            json!({"ok":true,"result":true}),
            json!({"ok":true,"result":true}),
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
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, "1").await.unwrap();
    let mut tx = db.begin().await.unwrap();
    let invite = store::invitations::create(&mut tx, "1").await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        show_invitation(&db, &telegram, "1", 1, "42", &invite.command)
            .await
            .unwrap()
    );
    assert!(!store::invitations::available(&db, "42").await.unwrap());
    let callback_data = {
        let captured = mock.lock().await;
        assert_eq!(captured.0[0].0, "sendMessage");
        let sent = &captured.0[0].1;
        assert_eq!(sent["chat_id"], 42);
        assert!(
            sent["text"]
                .as_str()
                .unwrap()
                .contains("bot operator can see")
        );
        assert!(!sent["text"].as_str().unwrap().contains(&invite.command));
        let button = &sent["reply_markup"]["inline_keyboard"][0][0];
        assert_eq!(button["text"], "Accept invitation");
        assert!(
            button["callback_data"]
                .as_str()
                .unwrap()
                .starts_with("invite-accept/insights/")
        );
        button["callback_data"].clone()
    };
    for user in [43, 42] {
        let query: callback::Query = serde_json::from_value(json!({
            "id":format!("callback-{user}"),"from":{"id":user,"is_bot":false},
            "message":{"message_id":70,"chat":{"id":user,"type":"private"}},"data":callback_data,
        }))
        .unwrap();
        handle_callback(&db, &telegram, &query, || {})
            .await
            .unwrap();
    }
    assert!(!store::invitations::available(&db, "43").await.unwrap());
    assert!(store::invitations::available(&db, "42").await.unwrap());
    let consent: i64 =
        sqlx::query_scalar("SELECT insights_version FROM bot_invitations WHERE member='42'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(consent, 2);
    assert!(
        show_invitation(&db, &telegram, "1", 1, "42", &invite.command)
            .await
            .unwrap()
    );
    assert!(mock.lock().await.1.is_empty());
    server.abort();
}

#[tokio::test]
async fn sends_group_replies_to_the_registered_chat_topic_and_anchor() {
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([json!({"ok":true,"result":{"message_id":45}})]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let scope = crate::store::groups::GroupRoute {
        chat: -1020,
        topic: 12,
    };
    assert_eq!(
        telegram
            .send_group(scope, "Result", Some(44))
            .await
            .unwrap(),
        MessageDelivery::Sent(Some(45))
    );
    let captured = mock.lock().await;
    assert_eq!(captured.0[0].0, "sendMessage");
    assert_eq!(captured.0[0].1["chat_id"], -1020);
    assert_eq!(captured.0[0].1["message_thread_id"], 12);
    assert_eq!(captured.0[0].1["reply_parameters"]["message_id"], 44);
    server.abort();
}

#[tokio::test]
async fn uploads_telegram_files_and_retries_only_the_explicit_rate_limit() {
    use axum::{body::Bytes, http::HeaderMap};
    let mock = Arc::new(Mutex::new(VecDeque::from([
        json!({"ok":false,"error_code":429,"parameters":{"retry_after":7}}).to_string(),
        json!({"ok":true,"result":{"message_id":45}}).to_string(),
        json!({"ok":false,"error_code":400}).to_string(),
        "gateway returned an unreadable response".into(),
    ])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new().route(
        "/botTEST/sendDocument",
        post(move |headers: HeaderMap, body: Bytes| {
            let mock = mock.clone();
            async move {
                let boundary = headers["content-type"]
                    .to_str()
                    .unwrap()
                    .strip_prefix("multipart/form-data; boundary=")
                    .unwrap();
                let body = std::str::from_utf8(&body).unwrap();
                assert!(body.contains(&format!("--{boundary}")));
                assert!(body.contains("name=\"document\"; filename=\"report.csv\""));
                assert!(body.contains("column\nvalue"));
                assert!(body.contains("\"message_id\":44"));
                mock.lock().await.pop_front().unwrap()
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let upload =
        || telegram.upload_file("42", "document", "report.csv", b"column\nvalue", Some(44));
    assert_eq!(upload().await.unwrap(), MessageDelivery::Retry(7));
    assert_eq!(upload().await.unwrap(), MessageDelivery::Sent(Some(45)));
    assert!(!upload().await.unwrap_err().is::<UploadUncertain>());
    assert!(upload().await.unwrap_err().is::<UploadUncertain>());
    server.abort();
}

async fn record(
    State(mock): State<Mock>,
    Path(method): Path<String>,
    Json(data): Json<Value>,
) -> Json<Value> {
    let mut mock = mock.lock().await;
    mock.0.push((method, data));
    Json(mock.1.pop_front().expect("unexpected Telegram request"))
}

#[tokio::test]
async fn email_buttons_and_previews_use_native_telegram_messages() {
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}}),
            json!({"ok":true,"result":{"message_id":210}}),
            json!({"ok":true,"result":{"message_id":211}}),
            json!({"ok":true,"result":{"message_id":212}}),
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let id = crate::security::id();
    let read = format!("mail-read/{id}");
    let body = "New email in Work (INBOX).";
    assert_eq!(
        telegram.send("42", body, Some(&read), None).await.unwrap(),
        MessageDelivery::Retry(3)
    );
    assert_eq!(
        telegram.send("42", body, Some(&read), None).await.unwrap(),
        MessageDelivery::Sent(Some(210))
    );
    let text = "From: private@example.test\nSubject: **Confidential**\n\nSee https://example.test.";
    assert_eq!(
        telegram.send_plain("42", text, Some(210)).await.unwrap(),
        MessageDelivery::Sent(Some(211))
    );
    let send = format!("mail-send/{id}");
    assert_eq!(
        telegram
            .send("42", "Review the email draft.", Some(&send), None)
            .await
            .unwrap(),
        MessageDelivery::Sent(Some(212))
    );

    let guard = mock.lock().await;
    let calls = &guard.0;
    assert!(calls.iter().all(|(method, _)| method == "sendMessage"));
    assert_eq!(calls[0], calls[1]);
    assert_eq!(calls[1].1["text"], body);
    assert_eq!(
        calls[1].1["reply_markup"]["inline_keyboard"],
        json!([[{"text":"Read more","callback_data":read}]])
    );
    assert_eq!(
        calls[2].1,
        json!({
            "chat_id":42,
            "text":text,
            "link_preview_options":{"is_disabled":true},
            "reply_parameters":{"message_id":210,"allow_sending_without_reply":true}
        })
    );
    assert_eq!(
        calls[3].1["reply_markup"]["inline_keyboard"],
        json!([[
            {"text":"Approve once","callback_data":format!("{send}/once")},
            {"text":"Deny","callback_data":format!("{send}/deny")}
        ]])
    );
    server.abort();
}

#[tokio::test]
async fn mcp_approval_buttons_resume_only_the_owners_waiting_request() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["42", "43"] {
        store::seed(&db, user).await.unwrap();
    }
    sqlx::query("UPDATE users SET provider='codex' WHERE id='42'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO connectors(id,owner,name,endpoint,auth) VALUES('docs','42','Docs','https://example.test','public')")
        .execute(&db)
        .await
        .unwrap();
    store::ingest_telegram(&db, "42", 1, "42", "Find the roadmap", 70)
        .await
        .unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let task = store::claim(&db).await.unwrap().unwrap();
    let request = crate::security::id();
    let body = "Search Docs for the roadmap. This sends your search query to Docs.";
    store::pause(
        &db,
        &task,
        "approve",
        &json!({
            "call":{"name":"call_mcp","args":{"connector":"docs","tool":"search","arguments":{"query":"roadmap"}}},
            "mcp_approval":{"connector":"docs","tool":"search","epoch":0}
        }),
        body,
        Some(&format!("mcp-approval/{request}")),
        900,
        &request,
    )
    .await
    .unwrap();

    let update = |id: i64, user: i64, kind: &str, decision: &str| {
        json!({
            "update_id":id,
            "callback_query":{
                "id":format!("callback-{id}"),
                "from":{"id":user,"is_bot":false},
                "message":{"message_id":71,"chat":{"id":user,"type":kind}},
                "data":format!("mcp-approval/{request}/{decision}")
            }
        })
    };
    let updates = json!({"ok":true,"result":[
        update(2,43,"private","always"),
        update(3,42,"group","deny"),
        update(4,42,"private","always"),
        update(5,42,"private","deny")
    ]});
    let accepted = json!({"ok":true,"result":true});
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}}),
            json!({"ok":true,"result":{"message_id":71}}),
            updates,
            accepted.clone(),
            accepted.clone(),
            json!({"ok":false,"error_code":400,"description":"Bad Request: message to edit not found"}),
            accepted,
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(store::can_deliver(&db, &item).await.unwrap());
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Retry(3)
    );
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Sent(Some(71))
    );
    let (updates, offset) = telegram.receive(0).await.unwrap();
    assert_eq!(offset, 6);
    assert_eq!(updates.len(), 3);
    for update in updates {
        let Inbound::Callback(query) = update else {
            panic!("expected a private callback");
        };
        handle_callback(&db, &telegram, &query, || {})
            .await
            .unwrap();
    }
    assert!(!store::can_deliver(&db, &item).await.unwrap());
    let next = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(next.id, task.id);
    assert_eq!(next.revision, task.revision);
    assert_eq!(
        store::claim_approval(&db, &next).await.unwrap().unwrap().id,
        request
    );

    let mock = mock.lock().await;
    let calls = &mock.0;
    assert_eq!(calls[0], calls[1]);
    assert_eq!(
        calls[1],
        (
            "sendMessage".into(),
            json!({
                "chat_id":42,"text":body,"link_preview_options":{"is_disabled":true},
                "reply_markup":{"inline_keyboard":[
                    [
                        {"text":"Approve once","callback_data":format!("mcp-approval/{request}/once")},
                        {"text":"Always approve","callback_data":format!("mcp-approval/{request}/always")}
                    ],
                    [{"text":"Deny","callback_data":format!("mcp-approval/{request}/deny")}]
                ]}
            })
        )
    );
    assert_eq!(calls[3].1["text"], "This option is no longer available.");
    assert_eq!(calls[4].1["text"], "Always approved for this tool.");
    assert_eq!(
        calls[5],
        (
            "editMessageReplyMarkup".into(),
            json!({
                "chat_id":42,"message_id":71,"reply_markup":{"inline_keyboard":[]}
            })
        )
    );
    assert_eq!(calls[6].1["text"], "This option is no longer available.");
    server.abort();
}

#[tokio::test]
async fn polling_extracts_private_quotes_and_only_new_user_emoji() {
    let user = json!({"id":42,"is_bot":false});
    let chat = json!({"id":42,"type":"private"});
    let bot = json!({"id":99,"is_bot":true});
    let updates = json!({"ok":true,"result":[
        {"update_id":1,"message":{"message_id":30,"chat":chat,"from":user,"text":"👍",
            "reply_to_message":{"message_id":29,"chat":chat,"from":bot,"text":"A whole earlier reply"},
            "quote":{"text":"earlier reply","position":8}}},
        {"update_id":2,"message":{"message_id":31,"chat":chat,"from":user,"text":"Hi",
            "reply_to_message":{"message_id":18,"chat":{"id":100,"type":"private"},"from":bot,"text":"Other chat"}}},
        {"update_id":3,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[],"new_reaction":[{"type":"emoji","emoji":"👍"}],"date":1}},
        {"update_id":4,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[{"type":"emoji","emoji":"👍"}],"new_reaction":[],"date":1}},
        {"update_id":5,"message_reaction":{"chat":{"id":42,"type":"group"},"message_id":29,"user":user,
            "old_reaction":[],"new_reaction":[{"type":"emoji","emoji":"👍"}],"date":1}},
        {"update_id":6,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[{"type":"emoji","emoji":"👍"}],
            "new_reaction":[{"type":"emoji","emoji":"👍"},{"type":"emoji","emoji":"🙉"}],"date":1}},
        {"update_id":7,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[{"type":"emoji","emoji":"👍"},{"type":"emoji","emoji":"🙉"}],
            "new_reaction":[{"type":"emoji","emoji":"🙉"},{"type":"emoji","emoji":"👍"}],"date":1}},
        {"update_id":8,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[{"type":"emoji","emoji":"🙉"}],"new_reaction":[],"date":1}},
        {"update_id":9,"message_reaction":{"chat":chat,"message_id":29,"user":{"id":43,"is_bot":false},
            "old_reaction":[],"new_reaction":[{"type":"emoji","emoji":"🙉"}],"date":1}},
        {"update_id":10,"message_reaction":{"chat":chat,"message_id":29,"user":{"id":42,"is_bot":true},
            "old_reaction":[],"new_reaction":[{"type":"emoji","emoji":"🙉"}],"date":1}},
        {"update_id":11,"message_reaction":{"chat":chat,"message_id":29,"actor_chat":{"id":42,"type":"private"},
            "old_reaction":[],"new_reaction":[{"type":"emoji","emoji":"🙉"}],"date":1}},
        {"update_id":12,"message_reaction":{"chat":chat,"message_id":29,"user":user,
            "old_reaction":[],"new_reaction":[{"type":"custom_emoji","custom_emoji_id":"123"}],"date":1}},
    ]});
    let mock: Mock = Arc::new(Mutex::new((Vec::new(), VecDeque::from([updates]))));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (updates, offset) = telegram.receive(0).await.unwrap();
    assert_eq!(offset, 13);
    let [
        Inbound::Message {
            text,
            reply: Some(quote),
            ..
        },
        Inbound::Message { reply: None, .. },
        Inbound::Reaction {
            user,
            message_id: 29,
            emoji,
            ..
        },
        Inbound::Reaction {
            external_id: 6,
            user: audio_actor,
            message_id: 29,
            emoji: audio_emoji,
        },
    ] = updates.as_slice()
    else {
        panic!("expected private replies and one event per newly added ordinary reaction")
    };
    assert_eq!(text, "👍");
    assert_eq!(quote.message_id, 29);
    assert_eq!(quote.excerpt.as_deref(), Some("earlier reply"));
    assert!(quote.from_bot);
    assert_eq!(user, "42");
    assert_eq!(emoji, "👍");
    assert_eq!(audio_actor, "42");
    assert_eq!(audio_emoji, "🙉");
    assert!(
        mock.lock().await.0[0].1["allowed_updates"]
            .as_array()
            .unwrap()
            .contains(&json!("message_reaction"))
    );
    server.abort();
}

#[tokio::test]
async fn reactions_use_the_latest_private_telegram_message_id() {
    let updates = json!({"ok":true,"result":[
        {"update_id":800,"message":{"message_id":51,"chat":{"id":42,"type":"private"},
            "from":{"id":42,"is_bot":false},"text":"Start a task"}},
        {"update_id":801,"message":{"message_id":52,"chat":{"id":-42,"type":"group"},
            "from":{"id":42,"is_bot":false},"text":"Ignore group messages"}}
    ]});
    let callback = json!({"ok":false,"error_code":429,"parameters":{"retry_after":3}});
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([updates, callback, json!({"ok":true,"result":true})]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (updates, offset) = telegram.receive(0).await.unwrap();
    assert_eq!(offset, 802);
    let [
        Inbound::Message {
            external_id,
            user,
            text,
            telegram_message_id: Some(message_id),
            reply: None,
        },
    ] = updates.as_slice()
    else {
        panic!("expected one private Telegram message")
    };
    assert_eq!((*external_id, *message_id), (800, 51));

    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, user).await.unwrap();
    sqlx::query("UPDATE users SET provider='codex' WHERE id=?")
        .bind(user)
        .execute(&db)
        .await
        .unwrap();
    store::ingest_telegram(&db, user, *external_id, user, text, *message_id)
        .await
        .unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let task = store::claim(&db).await.unwrap().unwrap();

    assert!(store::REACTION_EMOJIS.contains(&"👍"));
    assert!(store::queue_reaction(&db, &task, "✅").await.is_err());
    let mut other = task.clone();
    other.user = "43".into();
    assert!(!store::queue_reaction(&db, &other, "👀").await.unwrap());
    assert!(store::queue_reaction(&db, &task, "❤️").await.unwrap());
    assert!(!store::queue_reaction(&db, &task, "🔥").await.unwrap());
    let item = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(
        send_outbound(&db, &telegram, &item).await.unwrap(),
        MessageDelivery::Retry(3)
    );
    store::recover(&db).await.unwrap();
    let recovered = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(recovered.id, item.id);
    let task = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(
        send_outbound(&db, &telegram, &recovered).await.unwrap(),
        MessageDelivery::Sent(None)
    );
    sqlx::query("DELETE FROM outbox WHERE id=?")
        .bind(item.id)
        .execute(&db)
        .await
        .unwrap();
    assert!(!store::queue_reaction(&db, &task, "🔥").await.unwrap());

    store::ingest(&db, user, "mock", 1, user, "Synthetic follow-up")
        .await
        .unwrap();
    assert!(!store::queue_reaction(&db, &task, "👍").await.unwrap());
    assert_eq!(store::reaction_target(&db, &item).await.unwrap(), None);
    store::settle(&db, &task, "idle", None).await.unwrap();
    let next = store::claim(&db).await.unwrap().unwrap();
    assert!(!store::queue_reaction(&db, &next, "👍").await.unwrap());
    assert!(store::claim_outbox(&db).await.unwrap().is_none());

    let mock = mock.lock().await;
    let calls = &mock.0;
    let reaction = json!({"chat_id":42,"message_id":51,"reaction":[{"type":"emoji","emoji":"❤"}]});
    assert_eq!(calls[1], ("setMessageReaction".into(), reaction));
    assert_eq!(calls[1], calls[2]);
    server.abort();
}

#[tokio::test]
async fn drafts_use_native_entities_without_a_source_keyboard() {
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            json!({"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}),
            json!({"ok":true}),
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":4}}),
            json!({"ok":false,"error_code":404,"description":"Not Found"}),
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api,
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let body = "**Gevonden**. Source: https://example.test/docs";
    assert_eq!(telegram.draft("42", 5, body).await.unwrap(), None);
    assert_eq!(
        telegram.draft("42", 5, body).await.unwrap(),
        Some(Duration::from_secs(4))
    );
    assert!(telegram.draft("42", 5, body).await.is_err());

    let mock = mock.lock().await;
    assert_eq!(mock.0.len(), 4);
    for (method, payload) in &mock.0 {
        assert_eq!(method, "sendMessageDraft");
        assert_eq!(payload["draft_id"], 5);
        assert_eq!(
            payload["text"],
            "Gevonden.\n\nSource: https://example.test/docs"
        );
        assert!(payload.get("reply_markup").is_none());
    }
    assert_eq!(
        mock.0[0].1["entities"],
        json!([{"type":"bold","offset":0,"length":8}])
    );
    assert!(mock.0[1].1.get("entities").is_none());
    server.abort();
}

#[tokio::test]
async fn sends_formatted_replies_and_keeps_original_reply_buttons_on_rejection() {
    let accepted = json!({"ok":true,"result":{"message_id":201}});
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            accepted.clone(),
            json!({"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}),
            json!({"ok":false,"error_code":400,"description":"Bad Request: BUTTON_URL_INVALID"}),
            accepted.clone(),
            json!({"ok":false,"error_code":400,"description":"Bad Request: BUTTON_URL_INVALID"}),
            accepted,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":4}}),
            json!({"ok":false,"error_code":403,"description":"Forbidden"}),
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api,
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let body = "😀 **Gevonden**. Bronnen: [Documentatie](https://example.test/docs)";
    let replies = [store::SuggestedReply {
        token: "12345678-1234-4234-8234-123456789abc".into(),
        body: "Meer uitleg".into(),
    }];

    assert_eq!(
        telegram.send("42", body, None, None).await.unwrap(),
        MessageDelivery::Sent(Some(201))
    );
    for _ in 0..2 {
        assert_eq!(
            telegram
                .send_suggested_replies("42", body, &replies, Some(51))
                .await
                .unwrap(),
            MessageDelivery::Sent(Some(201))
        );
    }
    assert_eq!(
        telegram.send("42", body, None, None).await.unwrap(),
        MessageDelivery::Retry(4)
    );
    assert!(telegram.send("42", body, None, None).await.is_err());

    let mock = mock.lock().await;
    let calls = &mock.0;
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|(method, _)| method == "sendMessage"));
    let first = &calls[0].1;
    assert_eq!(first["text"], "😀 Gevonden.\n\nBronnen: Documentatie");
    assert_eq!(
        first["entities"][0],
        json!({"type":"bold","offset":3,"length":8})
    );
    let source = json!({"text":"Documentatie","url":"https://example.test/docs"});
    assert_eq!(first["reply_markup"]["inline_keyboard"], json!([[source]]));
    let with_suggestions = &calls[1].1;
    assert_eq!(
        with_suggestions["reply_parameters"],
        json!({"message_id":51,"allow_sending_without_reply":true})
    );
    assert_eq!(
        calls[2].1["reply_parameters"],
        with_suggestions["reply_parameters"]
    );
    let keyboard = &with_suggestions["reply_markup"]["inline_keyboard"];
    assert_eq!(
        keyboard[0][0]["callback_data"],
        format!("reply/{}", replies[0].token)
    );
    assert_eq!(keyboard[1][0], source);
    assert_eq!(calls[2].1["reply_markup"], with_suggestions["reply_markup"]);
    assert!(calls[2].1.get("entities").is_none());
    assert!(calls[3].1.get("entities").is_none());
    assert_eq!(calls[3].1["reply_markup"], suggested_keyboard(&replies));
    assert_eq!(
        calls[3].1["reply_parameters"],
        with_suggestions["reply_parameters"]
    );
    assert_eq!(calls[5].1["entities"], with_suggestions["entities"]);
    assert_eq!(calls[5].1["reply_markup"], suggested_keyboard(&replies));
    server.abort();
}

#[tokio::test]
async fn replies_to_a_voice_enabled_chat_keep_their_receipt_without_a_listen_button() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, "42").await.unwrap();
    sqlx::query("UPDATE users SET provider='openrouter',openrouter_key='fixture' WHERE id='42'")
        .execute(&db)
        .await
        .unwrap();
    store::ingest_telegram(&db, "42", 1, "42", "Wat staat er nog open?", 20)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    store::settle_reply(&db, &task, "Er staan geen taken open.", &[])
        .await
        .unwrap();

    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([json!({"ok":true,"result":{"message_id":201}})]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = url::Url::parse("https://tinybot.example").unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: origin.origin().ascii_serialization(),
        identity: Default::default(),
    };
    let router = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let vault = crate::security::Vault::new(&"f0".repeat(32)).unwrap();
    let app = crate::App {
        db: db.clone(),
        task_ready: Default::default(),
        delivery_ready: Default::default(),
        models: crate::model::Models::new(db.clone(), vault.clone(), "unused".into(), None, None),
        mcp: crate::mcp::Mcp::new(db.clone(), vault.clone(), origin),
        vault,
        token: "unused".into(),
        operator: "42".into(),
        origin: "https://tinybot.example".into(),
        anything_mcp_origin: None,
        forms: crate::forms::Forms::default(),
        workers: crate::workers::Workers {
            browser: "unused".into(),
            python: "unused".into(),
        },
        tools: vec![],
    };
    let worker = tokio::spawn(crate::channel::outbox(Arc::new(app), Arc::new(telegram)));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM outbox WHERE user='42' AND telegram_message_id=201
                           AND assistant_event_id IS NOT NULL AND state='sent')",
        )
        .fetch_one(&db)
        .await
        .unwrap()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    server.abort();

    let mock = mock.lock().await;
    assert_eq!(mock.0.len(), 1);
    let (method, request) = &mock.0[0];
    assert_eq!(method, "sendMessage");
    assert_eq!(request["text"], "Er staan geen taken open.");
    assert!(request.get("reply_markup").is_none());
}

#[tokio::test]
async fn sends_native_tables_and_falls_back_only_on_explicit_telegram_rejection() {
    let accepted = json!({"ok":true,"result":{"message_id":215}});
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            accepted.clone(),
            json!({"ok":false,"error_code":400,"description":"Bad Request: invalid rich message"}),
            accepted.clone(),
            json!({"ok":false,"error_code":404,"description":"Not Found"}),
            accepted,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":5}}),
            json!({"ok":false,"error_code":503,"description":"Service Unavailable"}),
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api,
        origin: "https://example.test".into(),
        identity: Default::default(),
    };
    let table = "| Abonnement | Maandprijs |\n| --- | ---: |\n| Lite | €15 |\n\n\
                 Bronnen: [Tarieven](https://example.test/tarieven)";
    let replies = [store::SuggestedReply {
        token: "12345678-1234-4234-8234-123456789abc".into(),
        body: "Meer uitleg".into(),
    }];

    for _ in 0..3 {
        assert_eq!(
            telegram
                .send_suggested_replies("42", table, &replies, Some(51))
                .await
                .unwrap(),
            MessageDelivery::Sent(Some(215))
        );
    }
    assert_eq!(
        telegram.send("42", table, None, None).await.unwrap(),
        MessageDelivery::Retry(5)
    );
    assert!(telegram.send("42", table, None, None).await.is_err());

    let mock = mock.lock().await;
    let calls = &mock.0;
    assert_eq!(
        calls
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>(),
        [
            "sendRichMessage",
            "sendRichMessage",
            "sendMessage",
            "sendRichMessage",
            "sendMessage",
            "sendRichMessage",
            "sendRichMessage"
        ]
    );
    let native = &calls[0].1;
    assert!(
        native["rich_message"]["html"]
            .as_str()
            .unwrap()
            .contains("<table bordered compact>")
    );
    assert_eq!(native["rich_message"]["skip_entity_detection"], true);
    let keyboard = &native["reply_markup"]["inline_keyboard"];
    assert_eq!(
        keyboard[0][0]["callback_data"],
        format!("reply/{}", replies[0].token)
    );
    assert_eq!(keyboard[1][0]["url"], "https://example.test/tarieven");
    for fallback in [&calls[2].1, &calls[4].1] {
        assert_eq!(fallback["reply_parameters"], native["reply_parameters"]);
        assert_eq!(fallback["reply_markup"], native["reply_markup"]);
        let text = fallback["text"].as_str().unwrap();
        assert!(text.contains("Lite") && text.contains("Maandprijs") && text.contains("€15"));
        assert!(!text.contains('|'));
    }
    server.abort();
}

#[tokio::test]
async fn sends_native_cards_and_direct_choices_with_honest_delivery_fallbacks() {
    let accepted = json!({"ok":true,"result":{"widget":{}}});
    let rejected = json!({"ok":false,"error_code":400,"description":"Bad Request: INPUT_MEDIA_WIDGET_NOT_ALLOWED"});
    assert_eq!(widget_rejection(&rejected), "widget_access_restricted");
    let rate_limit = json!({"ok":false,"error_code":429,"parameters":{"retry_after":12}});
    let mock: Mock = Arc::new(Mutex::new((
        Vec::new(),
        VecDeque::from([
            accepted.clone(),
            rate_limit,
            rejected.clone(),
            json!({"ok":true}),
            accepted,
            rejected.clone(),
            json!({"ok":true}),
            rejected,
            json!({"ok":true}),
            json!({"ok":true}),
            json!({"ok":true}),
            json!({"unexpected":"missing acknowledgement"}),
            json!({"ok":false,"error_code":503}),
        ]),
    )));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/botTEST", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api,
        origin: "https://example.test".into(),
        identity: Default::default(),
    };

    let card = Card::new("Hi 👋", "</p><script>&", &["A short item".into()]).unwrap();
    let replies = [store::SuggestedReply {
        token: "12345678-1234-4234-8234-123456789abc".into(),
        body: "Say it in Dutch".into(),
    }];
    assert_eq!(
        telegram.send_card("42", &card, &[], None).await.unwrap(),
        MessageDelivery::Sent(None)
    );
    assert_eq!(
        telegram.send_card("42", &card, &[], None).await.unwrap(),
        MessageDelivery::Retry(12)
    );
    assert_eq!(
        telegram
            .send_card("42", &card, &replies, Some(42))
            .await
            .unwrap(),
        MessageDelivery::Sent(None)
    );
    let choice = WidgetChoice {
        id: "12345678-1234-4234-8234-123456789abc".into(),
        label: "Pick a topic".into(),
        options: vec!["Travel".into(), "Cooking".into()],
    };
    assert_eq!(
        telegram.send_choice("42", &choice).await.unwrap(),
        MessageDelivery::Sent(None)
    );
    assert_eq!(
        telegram.send_choice("42", &choice).await.unwrap(),
        MessageDelivery::Sent(None)
    );
    assert_eq!(
        telegram
            .send("42", "Manage TinyBot", Some("panel"), None)
            .await
            .unwrap(),
        MessageDelivery::Sent(None)
    );
    assert_eq!(
        telegram
            .send("42", "Open settings", Some("mini"), None)
            .await
            .unwrap(),
        MessageDelivery::Sent(None)
    );
    let button = format!("form/{}", choice.id);
    assert_eq!(
        telegram
            .send("42", "I prepared a form for you.", Some(&button), None)
            .await
            .unwrap(),
        MessageDelivery::Sent(None)
    );
    assert!(
        telegram
            .send_card("42", &card, &[], None)
            .await
            .unwrap_err()
            .is::<DeliveryUncertain>()
    );
    assert!(
        telegram
            .send("42", "Fixture reply", None, None)
            .await
            .unwrap_err()
            .is::<DeliveryUncertain>()
    );

    let mock = mock.lock().await;
    let calls = &mock.0;
    assert_eq!(
        calls
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>(),
        [
            "sendWidget",
            "sendWidget",
            "sendWidget",
            "sendMessage",
            "sendWidget",
            "sendWidget",
            "sendMessage",
            "sendWidget",
            "sendMessage",
            "sendMessage",
            "sendMessage",
            "sendWidget",
            "sendMessage"
        ]
    );
    let html = calls[0].1["html"].as_str().unwrap();
    assert!(!html.contains("<script>"));
    assert!(html.contains("<h4>Hi 👋</h4>"));
    assert!(html.contains("&lt;/p&gt;&lt;script&gt;&amp;"));
    assert!(html.contains("<li>A short item</li>"));
    assert!(calls[0].1.get("reply_markup").is_none());
    assert_eq!(
        calls[3].1["text"],
        "Hi 👋\n\n</p><script>&\n\n• A short item"
    );
    assert_eq!(
        calls[3].1["entities"],
        json!([{"type":"bold","offset":0,"length":5}])
    );
    assert_eq!(calls[2].1["reply_markup"], calls[3].1["reply_markup"]);
    assert_eq!(
        calls[2].1["reply_parameters"],
        json!({"message_id":42,"allow_sending_without_reply":true})
    );
    assert_eq!(
        calls[3].1["reply_parameters"],
        calls[2].1["reply_parameters"]
    );
    assert_eq!(
        calls[3].1["reply_markup"]["inline_keyboard"][0][0]["text"],
        "Say it in Dutch"
    );
    let state: Value = serde_json::from_str(calls[4].1["chat_data"].as_str().unwrap()).unwrap();
    assert_eq!(state["options"], json!(["Travel", "Cooking"]));
    assert_eq!(calls[4].1["allow_queries"], true);
    assert_eq!(
        calls[6].1["reply_markup"]["inline_keyboard"][1][0]["callback_data"],
        format!("choice/{}/1", choice.id)
    );
    assert_eq!(
        calls[8].1["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
        "https://example.test/mini"
    );
    assert_eq!(
        calls[9].1["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
        "https://example.test/mini"
    );
    assert_eq!(
        calls[10].1["reply_markup"]["inline_keyboard"][0][0],
        json!({"text":"Open form","web_app":{"url":format!("https://example.test/mini?form={}",choice.id)}})
    );
    server.abort();
}
