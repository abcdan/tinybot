use super::{app_with_model, fixture};
use crate::{
    channel::{self, ChannelAdapter, Inbound, MessageDelivery},
    model::ModelProvider,
    runtime, store,
};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};

struct BriefReply;

#[async_trait]
impl ModelProvider for BriefReply {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        Ok(r#"{"kind":"reply","text":"A fixture explanation."}"#.to_owned())
    }
}

struct TelegramFixture {
    inbound: Mutex<mpsc::UnboundedReceiver<Vec<Inbound>>>,
    replies: mpsc::UnboundedSender<(String, String)>,
}

#[async_trait]
impl ChannelAdapter for TelegramFixture {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn receive(&self, offset: i64) -> Result<(Vec<Inbound>, i64)> {
        Ok((self.inbound.lock().await.recv().await.unwrap(), offset + 1))
    }

    async fn send(
        &self,
        user: &str,
        body: &str,
        _: Option<&str>,
        _: Option<i64>,
    ) -> Result<MessageDelivery> {
        self.replies.send((user.to_owned(), body.to_owned()))?;
        Ok(MessageDelivery::Sent(None))
    }
}

#[tokio::test]
async fn telegram_ingress_and_delivery_wake_workers_before_their_fallback_polls() {
    let db = fixture().await;
    let app = Arc::new(app_with_model(&db, Arc::new(BriefReply)));
    let (updates, inbound) = mpsc::unbounded_channel();
    let (replies, mut delivered) = mpsc::unbounded_channel();
    let telegram = Arc::new(TelegramFixture {
        inbound: Mutex::new(inbound),
        replies,
    });
    let workers = [
        tokio::spawn(channel::listen(app.clone(), "1".into(), telegram.clone())),
        tokio::spawn(runtime::run(app.clone(), telegram.clone())),
        tokio::spawn(channel::outbox(app, telegram)),
    ];

    let received_at = Instant::now();
    updates
        .send(
            [(1, "1", "What is Vulkan?"), (2, "2", "What is OpenGL?")]
                .into_iter()
                .map(|(id, user, text)| Inbound::Message {
                    external_id: id,
                    user: user.into(),
                    text: text.into(),
                    telegram_message_id: Some(id),
                    reply: None,
                })
                .collect(),
        )
        .unwrap();
    let first = tokio::time::timeout(Duration::from_millis(450), async {
        let mut answers = [
            delivered.recv().await.unwrap(),
            delivered.recv().await.unwrap(),
        ];
        answers.sort();
        answers
    })
    .await;
    eprintln!(
        "Telegram wake fixture: {} ms to both replies",
        received_at.elapsed().as_millis()
    );
    assert_eq!(
        first.expect("two independent replies should arrive before either fallback poll"),
        [
            ("1".into(), "A fixture explanation.".into()),
            ("2".into(), "A fixture explanation.".into()),
        ]
    );

    store::notify(&db, "2", "A durable item without a wake signal.", None)
        .await
        .unwrap();
    let fallback = tokio::time::timeout(Duration::from_secs(2), delivered.recv()).await;
    for worker in workers {
        worker.abort();
    }
    assert_eq!(
        fallback.unwrap().unwrap(),
        ("2".into(), "A durable item without a wake signal.".into())
    );
}

#[derive(Default)]
struct ResearchReply {
    started: tokio::sync::Notify,
    finish: tokio::sync::Notify,
}

#[async_trait]
impl ModelProvider for ResearchReply {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        self.started.notify_one();
        self.finish.notified().await;
        Ok(r#"{"kind":"reply","text":"Both sites list the same opening hours."}"#.into())
    }

    async fn complete_brief_reply(&self, _: &str, input: &str) -> Result<String> {
        let input: serde_json::Value = serde_json::from_str(input)?;
        let text = if input["progress"]["pages_read"] == 1 {
            "I checked the hours on one site and am comparing them."
        } else {
            "I’ll compare the published opening hours on both sites."
        };
        Ok(serde_json::json!({"text":text}).to_string())
    }
}

#[tokio::test]
async fn research_sends_a_generated_first_reply_then_one_confirmed_update() {
    let db = fixture().await;
    let model = Arc::new(ResearchReply::default());
    let app = Arc::new(app_with_model(&db, model.clone()));
    let (_updates, inbound) = mpsc::unbounded_channel();
    let (replies, mut delivered) = mpsc::unbounded_channel();
    let telegram = Arc::new(TelegramFixture {
        inbound: Mutex::new(inbound),
        replies,
    });
    let workers = [
        tokio::spawn(runtime::run(app.clone(), telegram.clone())),
        tokio::spawn(channel::outbox(app.clone(), telegram)),
    ];
    store::ingest_telegram(
        &db,
        "1",
        1,
        "1",
        "Research the opening hours on both websites.",
        21,
    )
    .await
    .unwrap();
    app.task_ready.notify_one();
    tokio::time::timeout(Duration::from_secs(3), model.started.notified())
        .await
        .unwrap();
    let make_due = || {
        sqlx::query("UPDATE outbox SET retry_at=0 WHERE button LIKE 'ack-generate/%'").execute(&db)
    };
    make_due().await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(3), delivered.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.1,
        "I’ll compare the published opening hours on both sites."
    );

    let (task, revision): (String, i64) =
        sqlx::query_as("SELECT id,revision FROM tasks WHERE user='1' AND state='running'")
            .fetch_one(&db)
            .await
            .unwrap();
    let trace = crate::audit::Trace::for_progress(&db, &task, "1", revision)
        .await
        .unwrap()
        .unwrap();
    crate::audit::scope(
        Some(trace),
        crate::audit::record(
            "tool",
            "browse",
            "completed",
            serde_json::json!({"page_bytes":64}),
        ),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM audit_deliveries WHERE kind='progress' AND state='sent')",
        )
        .fetch_one(&db)
        .await
        .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    make_due().await.unwrap();
    let update = tokio::time::timeout(Duration::from_secs(3), delivered.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        update.1,
        "I checked the hours on one site and am comparing them."
    );

    model.finish.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(3), delivered.recv())
        .await
        .unwrap()
        .unwrap();
    for worker in workers {
        worker.abort();
    }
    assert_eq!(answer.1, "Both sites list the same opening hours.");
}

#[derive(Default)]
struct PlainAfterDefer {
    brief: AtomicUsize,
    full: AtomicUsize,
}

const PLAIN_ANSWER: &str =
    "Entropie geeft aan hoeveel mogelijke toestanden bij een systeem passen.";

#[async_trait]
impl ModelProvider for PlainAfterDefer {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        unreachable!("the fixture uses the brief and full task interfaces")
    }

    async fn complete_brief_reply(&self, _: &str, _: &str) -> Result<String> {
        self.brief.fetch_add(1, Ordering::Relaxed);
        Ok(r#"{"kind":"defer"}"#.into())
    }

    async fn complete_turn(
        &self,
        _: &str,
        _: &str,
        tools: &[crate::model::ToolDefinition],
        delta: Option<&mut crate::model::TextDelta<'_>>,
    ) -> Result<String> {
        self.full.fetch_add(1, Ordering::Relaxed);
        assert!(!tools.is_empty());
        delta.expect("an extra buffered call would reproduce the regression")(PLAIN_ANSWER);
        Ok(PLAIN_ANSWER.into())
    }
}

#[tokio::test]
async fn a_deferred_quick_question_delivers_complete_prose_from_the_first_full_call() {
    let db = fixture().await;
    let model = Arc::new(PlainAfterDefer::default());
    let app = Arc::new(app_with_model(&db, model.clone()));
    let (updates, inbound) = mpsc::unbounded_channel();
    let (replies, mut delivered) = mpsc::unbounded_channel();
    let telegram = Arc::new(TelegramFixture {
        inbound: Mutex::new(inbound),
        replies,
    });
    let workers = [
        tokio::spawn(channel::listen(app.clone(), "1".into(), telegram.clone())),
        tokio::spawn(runtime::run(app.clone(), telegram.clone())),
        tokio::spawn(channel::outbox(app, telegram)),
    ];
    updates
        .send(vec![Inbound::Message {
            external_id: 1,
            user: "1".into(),
            text: "Wat is de entropie?".into(),
            telegram_message_id: Some(17),
            reply: None,
        }])
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(3), delivered.recv()).await;
    for worker in workers {
        worker.abort();
    }
    assert_eq!(answer.unwrap().unwrap(), ("1".into(), PLAIN_ANSWER.into()));
    assert_eq!(model.brief.load(Ordering::Relaxed), 1);
    assert_eq!(model.full.load(Ordering::Relaxed), 1);
    let detail: String = sqlx::query_scalar(
        "SELECT detail FROM audit_entries WHERE name='reply_format' AND status='accepted'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&detail).unwrap(),
        serde_json::json!({
            "format":"plain_text","response_bytes":PLAIN_ANSWER.len(),
        })
    );
}
