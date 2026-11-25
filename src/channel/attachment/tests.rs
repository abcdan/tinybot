use super::*;
use crate::channel::{Inbound, MessageDelivery, Telegram};
use async_trait::async_trait;
use axum::{Json, Router, routing::post};
use serde_json::json;
use std::sync::Mutex;
use tokio::net::TcpListener;

#[derive(Default)]
struct Downloads(Mutex<Vec<String>>);

#[async_trait]
impl ChannelAdapter for Downloads {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn receive(&self, _: i64) -> Result<(Vec<Inbound>, i64)> {
        unreachable!()
    }

    async fn send(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<MessageDelivery> {
        unreachable!()
    }

    async fn download_file(&self, file: &str, limit: usize) -> Result<Vec<u8>> {
        assert_eq!(limit, MAX_ATTACHMENT_BYTES);
        self.0.lock().unwrap().push(file.into());
        match file {
            "doc" => Ok(b"amount,city\n9,Utrecht\n".to_vec()),
            "photo" => Ok(b"\x89PNG\r\n\x1a\nfixture".to_vec()),
            _ => panic!("unexpected download"),
        }
    }
}

#[tokio::test]
async fn telegram_attachments_preserve_captions_and_skip_duplicate_or_unauthorized_downloads() {
    let updates = json!({"ok":true,"result":[
        {"update_id":1,"message":{"message_id":21,"chat":{"id":1,"type":"private"},
            "from":{"id":1,"is_bot":false},"document":{"file_id":"doc","file_name":"/dir/results.csv","mime_type":"text/csv"},
            "caption":"Check the totals","reply_to_message":{"message_id":20,"chat":{"id":1,"type":"private"},"text":"Previous question"}}},
        {"update_id":2,"message":{"message_id":22,"chat":{"id":1,"type":"private"},
            "from":{"id":1,"is_bot":false},"photo":[
                {"file_id":"thumb","width":20,"height":20},
                {"file_id":"photo","width":200,"height":100,"file_size":3000},
                {"file_id":"oversize","width":800,"height":400,"file_size":MAX_ATTACHMENT_BYTES + 1}]}},
        {"update_id":3,"message":{"message_id":23,"chat":{"id":1,"type":"private"},
            "from":{"id":1,"is_bot":false},"document":{"file_id":"oversize","file_size":MAX_ATTACHMENT_BYTES + 1}}},
        {"update_id":4,"message":{"message_id":24,"chat":{"id":2,"type":"private"},
            "from":{"id":2,"is_bot":false},"document":{"file_id":"unauthorized"}}},
        {"update_id":5,"message":{"message_id":25,"chat":{"id":-1,"type":"group"},
            "from":{"id":1,"is_bot":false},"document":{"file_id":"group"}}},
        {"update_id":6,"message":{"message_id":26,"chat":{"id":1,"type":"private"},
            "from":{"id":2,"is_bot":false},"document":{"file_id":"other-user"}}}
    ]});
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Telegram {
        client: reqwest::Client::new(),
        api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
        origin: "https://tinybot.test".into(),
        identity: Default::default(),
    };
    let router = Router::new().route(
        "/botTEST/getUpdates",
        post(move || async move { Json(updates) }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, "1").await.unwrap();
    sqlx::query("UPDATE users SET provider='codex' WHERE id='1'")
        .execute(&db)
        .await
        .unwrap();
    let vault = crate::security::Vault::new(&"aa".repeat(32)).unwrap();
    let app = App {
        db: db.clone(),
        task_ready: Default::default(),
        delivery_ready: Default::default(),
        vault: vault.clone(),
        token: "test".into(),
        operator: "1".into(),
        origin: telegram.origin.clone(),
        anything_mcp_origin: None,
        models: crate::model::Models::new(db.clone(), vault.clone(), "unused".into(), None, None),
        mcp: crate::mcp::Mcp::new(
            db.clone(),
            vault,
            url::Url::parse(&telegram.origin).unwrap(),
        ),
        forms: crate::forms::Forms::default(),
        workers: crate::workers::Workers {
            browser: "unused".into(),
            python: "unused".into(),
        },
        tools: Vec::new(),
    };
    let downloads = Downloads::default();
    for _ in 0..2 {
        let (messages, offset) = telegram.receive(0).await.unwrap();
        assert_eq!(offset, 7);
        assert_eq!(messages.len(), 4);
        for (index, message) in messages.into_iter().enumerate() {
            let Inbound::Attachment(received) = message else {
                panic!("expected attachment")
            };
            handle(&app, "1", &downloads, received).await.unwrap();
            if index == 0 {
                let task: crate::store::Task = sqlx::query_as("SELECT * FROM tasks WHERE user='1'")
                    .fetch_one(&db)
                    .await
                    .unwrap();
                if task.revision == 1 {
                    assert_eq!(
                        store::latest_reply_context(&db, &task.id)
                            .await
                            .unwrap()
                            .unwrap()
                            .excerpt
                            .as_deref(),
                        Some("Previous question"),
                    );
                }
            }
        }
    }
    assert_eq!(*downloads.0.lock().unwrap(), ["doc", "photo"]);
    let entries: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT e.body,a.name,a.mime FROM events e JOIN telegram_attachments a ON a.event=e.id ORDER BY e.id",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        entries,
        [
            (
                "Check the totals".into(),
                "results.csv".into(),
                "text/plain".into()
            ),
            (
                "Review the attached file.".into(),
                "photo.png".into(),
                "image/png".into()
            ),
        ]
    );
    let notification = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(notification.reply_to_message_id, Some(23));
    assert!(notification.body.contains("8 MB"));
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    server.abort();
}

#[test]
fn attachment_kind_is_validated_against_downloaded_bytes() {
    let pdf = Source::Document(Document {
        file_id: "doc".into(),
        file_size: None,
        file_name: Some("invoice.pdf".into()),
    });
    assert!(pdf.classify(b"\0unsupported binary").is_none());
    assert_eq!(
        pdf.classify(b"%PDF-1.4\n"),
        Some(("application/pdf", "invoice.pdf".into()))
    );
    assert_eq!(
        pdf.classify(b"\xff\xd8\xffjpeg"),
        Some(("image/jpeg", "invoice.pdf".into()))
    );
}
