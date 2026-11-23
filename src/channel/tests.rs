use super::{
    dispatch::send_outbound,
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
    ]});
    let mock: Mock = Arc::new(Mutex::new((Vec::new(), VecDeque::from([updates]))));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://example.test".into(),
    };
    let app = Router::new()
        .route("/botTEST/{method}", post(record))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (updates, offset) = telegram.receive(0).await.unwrap();
    assert_eq!(offset, 6);
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
    ] = updates.as_slice()
    else {
        panic!("expected private replies and one new reaction")
    };
    assert_eq!(text, "👍");
    assert_eq!(quote.message_id, 29);
    assert_eq!(quote.excerpt.as_deref(), Some("earlier reply"));
    assert!(quote.from_bot);
    assert_eq!(user, "42");
    assert_eq!(emoji, "👍");
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
    assert_eq!(calls.len(), 7);
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
    assert_eq!(calls[4].1["entities"], with_suggestions["entities"]);
    assert_eq!(calls[4].1["reply_markup"], suggested_keyboard(&replies));
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
