use super::{app_with_model, fixture};
use crate::{
    channel::{self, ChannelAdapter, Inbound, MessageDelivery},
    model::ModelProvider,
    runtime, store,
};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    sync::Arc,
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
