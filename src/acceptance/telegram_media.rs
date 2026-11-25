use super::*;
use crate::{
    audit,
    channel::Telegram,
    store::{
        acknowledgements,
        outputs::{self, MediaDelivery, MediaFormat},
    },
};
use axum::{Router, body::Bytes, extract::Path, routing::post};
use std::collections::VecDeque;
use tokio::{net::TcpListener, sync::Mutex};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nprivate screenshot";
const MP4: &[u8] = b"\x00\x00\x00\x18ftypisomprivate video";
const SCREENSHOT: &str = "screenshot_page";
const RECORD: &str = "record_page";

struct QueueOnlyChief;

#[async_trait]
impl ModelProvider for QueueOnlyChief {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!("captured media has already been queued")
    }
}

async fn start(db: &SqlitePool, task: &store::Task, action: &str) {
    assert!(store::start_action(db, task, action).await.unwrap());
}

fn media(tool: &'static str, format: MediaFormat, bytes: &[u8]) -> MediaDelivery {
    MediaDelivery {
        tool,
        format,
        bytes: bytes.into(),
        finish_turn: false,
        cookie_dialog: None,
    }
}

async fn due_progress(db: &SqlitePool) {
    sqlx::query("UPDATE outbox SET retry_at=0 WHERE button LIKE 'ack-generate/%'")
        .execute(db)
        .await
        .unwrap();
}

async fn queue_screenshot(app: &App, task: &store::Task, finish_turn: bool) {
    start(&app.db, task, "image").await;
    let mut screenshot = media(SCREENSHOT, MediaFormat::Png, PNG);
    screenshot.finish_turn = finish_turn;
    assert!(
        outputs::queue_media(&app.db, &app.vault, task, "image", &screenshot)
            .await
            .unwrap()
    );
    assert_eq!(
        task_for(&app.db, &task.id).await.state,
        if finish_turn { "idle" } else { "running" }
    );
}

#[tokio::test]
async fn media_skips_progress_that_is_waiting_or_still_being_written() {
    for (generating, finish_turn) in [(false, true), (true, false)] {
        let db = fixture().await;
        let app = app_with_model(&db, Arc::new(QueueOnlyChief));
        store::ingest_telegram(&db, "1", 1, "1", "Capture the page.", 27)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        let generation = if generating {
            due_progress(&db).await;
            Some(acknowledgements::claim(&db).await.unwrap().unwrap())
        } else {
            None
        };
        queue_screenshot(&app, &task, finish_turn).await;
        let progress: String =
            sqlx::query_scalar("SELECT state FROM audit_deliveries WHERE kind='progress'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(progress, "skipped");
        let image = store::claim_delivery(&db).await.unwrap().unwrap();
        assert_eq!(image.button.as_deref(), Some(outputs::BUTTON));
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(image.id)
            .execute(&db)
            .await
            .unwrap();

        if let Some(generation) = generation {
            acknowledgements::finish(&db, &generation, Some("I’ll capture the page."), true)
                .await
                .unwrap();
        }
        due_progress(&db).await;
        assert!(acknowledgements::claim(&db).await.unwrap().is_none());
        assert!(store::claim_delivery(&db).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn a_written_acknowledgement_takes_its_delivery_turn_before_media() {
    for sending in [false, true] {
        let db = fixture().await;
        let app = app_with_model(&db, Arc::new(QueueOnlyChief));
        store::ingest_telegram(&db, "1", 1, "1", "Capture the page.", 27)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        due_progress(&db).await;
        let first = acknowledgements::claim(&db).await.unwrap().unwrap();
        acknowledgements::finish(&db, &first, Some("I’ll capture the page."), true)
            .await
            .unwrap();
        let claimed = if sending {
            Some(store::claim_delivery(&db).await.unwrap().unwrap())
        } else {
            None
        };
        queue_screenshot(&app, &task, true).await;
        let progress = match claimed {
            Some(progress) => progress,
            None => store::claim_delivery(&db).await.unwrap().unwrap(),
        };
        assert_eq!(progress.id, first.id);
        assert!(acknowledgements::ready(&db, &progress).await.unwrap());
        assert!(store::claim_delivery(&db).await.unwrap().is_none());
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(progress.id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            store::claim_delivery(&db)
                .await
                .unwrap()
                .unwrap()
                .button
                .as_deref(),
            Some(outputs::BUTTON)
        );
        due_progress(&db).await;
        assert!(acknowledgements::claim(&db).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn delivers_ephemeral_media_with_retry_then_removes_the_ciphertext() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(QueueOnlyChief)));
    store::ingest_telegram(&db, "1", 1, "1", "Capture the page.", 27)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    let session = audit::sessions(&db, "1", None).await.unwrap()["sessions"][0]["id"]
        .as_i64()
        .unwrap();
    start(&db, &task, "image").await;
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "image",
            &media(SCREENSHOT, MediaFormat::Png, PNG),
        )
        .await
        .unwrap()
    );
    start(&db, &task, "same-image").await;
    assert!(
        !outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "same-image",
            &media(SCREENSHOT, MediaFormat::Png, PNG),
        )
        .await
        .unwrap()
    );
    start(&db, &task, "video").await;
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "video",
            &media(RECORD, MediaFormat::Mp4, MP4),
        )
        .await
        .unwrap()
    );

    let expected = Arc::new(Mutex::new(VecDeque::from([
        (
            "sendPhoto",
            "photo",
            "image.png",
            "image/png",
            PNG,
            json!({"ok":false,"error_code":429,"parameters":{"retry_after":180}}),
        ),
        (
            "sendPhoto",
            "photo",
            "image.png",
            "image/png",
            PNG,
            json!({"ok":false,"error_code":400,"description":"Bad Request: PHOTO_INVALID_DIMENSIONS"}),
        ),
        (
            "sendDocument",
            "document",
            "image.png",
            "image/png",
            PNG,
            json!({"ok":true,"result":{"message_id":70}}),
        ),
        (
            "sendVideo",
            "video",
            "recording.mp4",
            "video/mp4",
            MP4,
            json!({"ok":true,"result":{"message_id":71}}),
        ),
    ])));
    let replies = expected.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let telegram = Arc::new(Telegram::fixture(format!(
        "http://{}/botTEST",
        listener.local_addr().unwrap()
    )));
    let router = Router::new().route(
        "/botTEST/{method}",
        post(move |Path(method): Path<String>, body: Bytes| {
            let replies = replies.clone();
            async move {
                let (expected_method, field, name, mime, bytes, response) =
                    replies.lock().await.pop_front().unwrap();
                assert_eq!(method, expected_method);
                let part = format!(
                    "name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n"
                );
                assert!(
                    body.windows(part.len())
                        .any(|window| window == part.as_bytes())
                );
                assert!(body.windows(bytes.len()).any(|window| window == bytes));
                assert!(String::from_utf8_lossy(&body).contains("\"message_id\":27"));
                axum::Json(response)
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let worker = tokio::spawn(crate::channel::outbox(app.clone(), telegram));
    let (image, encrypted) =
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(item) = sqlx::query_as::<_, (i64, String)>(
                "SELECT o.id,f.ciphertext FROM outbox o JOIN telegram_outputs f ON f.outbox=o.id
                 WHERE f.media_format='png' AND o.state='queued' AND o.retry_at>0",
            ).fetch_optional(&db).await.unwrap() {
                break item;
            }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    assert!(!encrypted.contains("private screenshot"));
    sqlx::query("UPDATE outbox SET retry_at=0 WHERE id=?")
        .bind(image)
        .execute(&db)
        .await
        .unwrap();
    app.delivery_ready.notify_one();
    let receipts = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let sent = sqlx::query_as::<_, (i64, Option<String>)>(
                "SELECT o.telegram_message_id,f.ciphertext FROM outbox o
                 JOIN telegram_outputs f ON f.outbox=o.id WHERE o.state='sent' ORDER BY o.id",
            )
            .fetch_all(&db)
            .await
            .unwrap();
            if sent.len() == 2 {
                break sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receipts, [(70, None), (71, None)]);
    let export = audit::export(&db, "1", session).await.unwrap();
    let delivery = &export["delivery"];
    assert_eq!(delivery["timing"]["result_status"], "delivered");
    assert!(delivery["timing"]["result_delivery_ms"].is_number());
    let audit_receipts: Vec<_> = delivery["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|receipt| receipt["kind"] == "reply")
        .collect();
    assert_eq!(audit_receipts.len(), 2);
    for receipt in &audit_receipts {
        assert_eq!(receipt["state"], "sent");
        assert!(receipt["finished_at_ms"].is_number());
    }
    assert_eq!(audit_receipts[0]["deferrals"], 1);
    assert!(expected.lock().await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM task_artifacts")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn does_not_deliver_superseded_media_and_prunes_expired_retries() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(QueueOnlyChief));
    store::ingest_telegram(&db, "1", 1, "1", "Capture the page.", 27)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    start(&db, &task, "image").await;
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "image",
            &media(SCREENSHOT, MediaFormat::Png, MP4),
        )
        .await
        .is_err()
    );
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "image",
            &media(SCREENSHOT, MediaFormat::Png, PNG),
        )
        .await
        .unwrap()
    );
    let image = store::claim_delivery(&db).await.unwrap().unwrap();
    assert!(
        outputs::load(&db, &app.vault, &image)
            .await
            .unwrap()
            .is_some()
    );
    start(&db, &task, "video").await;
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "video",
            &media(RECORD, MediaFormat::Mp4, MP4),
        )
        .await
        .unwrap()
    );
    start(&db, &task, "superseded").await;
    store::ingest_telegram(&db, "1", 2, "1", "Change the screenshot.", 28)
        .await
        .unwrap();
    assert!(
        outputs::load(&db, &app.vault, &image)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        outputs::queue_media(
            &db,
            &app.vault,
            &task,
            "superseded",
            &media(SCREENSHOT, MediaFormat::Png, PNG),
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE outbox SET state='skipped' WHERE id=?")
        .bind(image.id)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE telegram_outputs SET media_expires_at=0 WHERE media_format='mp4'")
        .execute(&db)
        .await
        .unwrap();
    store::expire(&db).await.unwrap();
    let terminal: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT o.state,f.ciphertext FROM outbox o JOIN telegram_outputs f ON f.outbox=o.id ORDER BY o.id",
    ).fetch_all(&db).await.unwrap();
    assert_eq!(
        terminal,
        [("skipped".into(), None), ("failed".into(), None)]
    );
}

#[tokio::test]
async fn media_enqueue_and_action_completion_are_atomic() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(QueueOnlyChief));
    store::ingest_telegram(&db, "1", 1, "1", "Capture the page.", 27)
        .await
        .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    start(&db, &task, "already-finished").await;
    store::finish_action(&db, &task, "already-finished", "Done.", false)
        .await
        .unwrap();
    let mut screenshot = media(SCREENSHOT, MediaFormat::Png, PNG);
    screenshot.finish_turn = true;
    for action in ["missing", "already-finished"] {
        assert!(
            outputs::queue_media(&db, &app.vault, &task, action, &screenshot)
                .await
                .is_err()
        );
    }
    let counts: (i64, i64, i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM outbox WHERE button='telegram-output'),
                (SELECT count(*) FROM telegram_outputs),
                (SELECT count(*) FROM audit_deliveries WHERE kind='reply'),
                (SELECT state FROM audit_deliveries WHERE kind='progress')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 0, "queued".into()));
    assert_eq!(task_for(&db, &task.id).await.state, "running");

    start(&db, &task, "active").await;
    assert!(
        outputs::queue_media(&db, &app.vault, &task, "active", &screenshot)
            .await
            .unwrap()
    );
    assert_eq!(task_for(&db, &task.id).await.state, "idle");
    let result: (String, String) = sqlx::query_as(
        "SELECT p.state,e.body FROM pending p JOIN events e ON e.task=p.task
         WHERE p.id='active' AND e.role='tool' ORDER BY e.id DESC LIMIT 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(result.0, "done");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.1).unwrap(),
        json!({"queued":true,"media_delivery":SCREENSHOT})
    );
}
