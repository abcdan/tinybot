use super::{ENGINE, MAX_REPLY_BYTES, Step};
use crate::{
    audit,
    channel::ChannelAdapter,
    model::{ModelProvider, ToolDefinition, parse_json_reply},
    security::clip,
    store::{self, Task},
};
use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;
use std::{sync::Mutex, time::Duration};
use tokio::{
    sync::Notify,
    time::{Instant, MissedTickBehavior, interval_at},
};

const MAX_ENVELOPE_BYTES: usize = 24_000;

#[derive(Default)]
pub(super) struct Feedback {
    source: Mutex<String>,
    stale: Notify,
    pub(super) delivery: tokio::sync::Mutex<()>,
}

impl Feedback {
    pub(super) async fn complete(
        &self,
        model: &dyn ModelProvider,
        input: &Value,
        tools: &[ToolDefinition],
    ) -> Result<Option<Step>> {
        self.source.lock().expect("draft buffer").clear();
        let input = input.to_string();
        let mut append = |text: &str| {
            let mut source = self.source.lock().expect("draft buffer");
            let remaining = MAX_ENVELOPE_BYTES - source.len();
            if remaining >= 4 {
                source.push_str(clip(text, remaining));
            }
        };
        let streamed = tokio::select! {
            biased;
            _ = self.stale.notified() => return Ok(None),
            result = audit::model("agent_turn_stream", async {
                let response = model.complete_turn(ENGINE, &input, tools, Some(&mut append)).await?;
                parse_json_reply(&response)
            }) => result,
        };
        if let Ok(step) = streamed {
            return Ok(Some(step));
        }

        self.source.lock().expect("draft buffer").clear();
        tracing::debug!("streamed model response failed; retrying with buffered completion");
        let buffered = tokio::select! {
            biased;
            _ = self.stale.notified() => return Ok(None),
            result = audit::model("agent_turn_buffered", async {
                let response = model.complete_turn(ENGINE, &input, tools, None).await?;
                parse_json_reply(&response)
            }) => result?,
        };
        Ok(Some(buffered))
    }

    pub(super) async fn show(&self, db: &SqlitePool, task: &Task, channel: &dyn ChannelAdapter) {
        let start = Instant::now() + Duration::from_millis(400);
        let mut interval = interval_at(start, Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut next_typing = start;
        let mut retry_at = start;
        let mut last_draft = String::new();
        let draft_id = (rand::random::<i32>() & i32::MAX).max(1);
        let mut draft_supported = true;
        loop {
            interval.tick().await;
            let _delivery = self.delivery.lock().await;
            match store::current(db, task).await {
                Ok(true) => {}
                Ok(false) => {
                    self.stale.notify_one();
                    return;
                }
                Err(_) => continue,
            }
            let now = Instant::now();
            if now < retry_at {
                continue;
            }
            let draft = reply_preview(&self.source.lock().expect("draft buffer"));
            if draft_supported
                && let Some(text) = draft
                && text != last_draft
            {
                match channel.draft(&task.user, draft_id, &text).await {
                    Ok(None) => last_draft = text,
                    Ok(Some(delay)) => retry_at = Instant::now() + delay,
                    Err(_) => draft_supported = false,
                }
            } else if last_draft.is_empty() && now >= next_typing {
                next_typing = now + Duration::from_millis(3500);
                if let Ok(Some(delay)) = channel.typing(&task.user).await {
                    retry_at = Instant::now() + delay;
                }
            }
        }
    }
}

// Accept only the user-visible text string of a reply. Tool envelopes, fields
// nested in tool arguments, and provider reasoning never produce a preview.
fn reply_preview(source: &str) -> Option<String> {
    let source = source.trim_start();
    let source = source
        .strip_prefix("```json")
        .or_else(|| source.strip_prefix("```"))
        .unwrap_or(source)
        .trim_start();
    let source = source
        .strip_prefix('{')?
        .trim_start()
        .strip_prefix("\"kind\"")?;
    let source = source
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix("\"reply\"")?;
    let source = source
        .trim_start()
        .strip_prefix(',')?
        .trim_start()
        .strip_prefix("\"text\"")?;
    let source = source
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let prefix = source.get(..complete_string_prefix(source))?;
    let text: String = serde_json::from_str(&format!("\"{prefix}\"")).ok()?;
    (!text.trim().is_empty()).then(|| clip(&text, MAX_REPLY_BYTES).to_owned())
}

// Locate the largest complete string prefix without cutting an escape or a
// surrogate pair in half when the provider splits one across deltas.
fn complete_string_prefix(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut offset = 0;
    while let Some(character) = bytes.get(offset) {
        match character {
            b'"' => break,
            b'\\' => {
                let width = match bytes.get(offset + 1) {
                    Some(b'u') => {
                        let Some(digits) = source.get(offset + 2..offset + 6) else {
                            break;
                        };
                        let Ok(code) = u16::from_str_radix(digits, 16) else {
                            break;
                        };
                        if (0xd800..=0xdbff).contains(&code) {
                            12
                        } else {
                            6
                        }
                    }
                    Some(_) => 2,
                    None => break,
                };
                if offset + width > bytes.len() {
                    break;
                }
                offset += width;
            }
            _ => offset += 1,
        }
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{channel::Inbound, model::TextDelta};
    use async_trait::async_trait;
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::{
        future::pending,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn previews_only_the_reply_string_as_its_escapes_arrive() {
        assert_eq!(
            reply_preview(r#"{"kind":"reply","text":"Hello\"#).as_deref(),
            Some("Hello")
        );
        assert_eq!(
            reply_preview(r#"{"kind":"reply","text":"Hello\n\uD83D"#).as_deref(),
            Some("Hello\n")
        );
        assert_eq!(
            reply_preview(r#"{"kind":"reply","text":"Hello\n\uD83D\uDC4B \"world\""}"#).as_deref(),
            Some("Hello\n👋 \"world\"")
        );
        assert_eq!(
            reply_preview("```json\n{ \"kind\" : \"reply\", \"text\" : \"Part").as_deref(),
            Some("Part")
        );
        assert!(reply_preview(r#"{"kind":"reply","text":"   "#).is_none());
    }

    #[test]
    fn ignores_tool_arguments_and_text_outside_the_reply() {
        assert!(
            reply_preview(r#"{"kind":"tool","name":"browse","args":{"text":"private"}}"#).is_none()
        );
        assert!(reply_preview(r#"{"text":"ambiguous","kind":"reply"}"#).is_none());
        assert_eq!(
            reply_preview(r#"{"kind":"reply","text":"visible","secret":"hidden"}"#).as_deref(),
            Some("visible")
        );
    }

    struct FallbackModel {
        streamed: Option<&'static str>,
        buffered: Option<&'static str>,
        calls: AtomicUsize,
        buffering: Notify,
    }

    #[async_trait]
    impl ModelProvider for FallbackModel {
        async fn complete(&self, _: &str, _: &str) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.buffering.notify_one();
            match self.buffered {
                Some(text) => Ok(text.into()),
                None => pending().await,
            }
        }

        async fn complete_streaming(
            &self,
            _: &str,
            _: &str,
            delta: &mut TextDelta<'_>,
        ) -> Result<String> {
            delta(r#"{"kind":"reply","text":"Incomplete"#);
            self.streamed
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("provider stream failed"))
        }
    }

    #[tokio::test]
    async fn retries_a_failed_or_invalid_stream_once_without_drafting_the_fallback() {
        for streamed in [None, Some(r#"{"kind":"reply","text":"Incomplete"#)] {
            let model = FallbackModel {
                streamed,
                buffered: Some(r#"{"kind":"reply","text":"Recovered"}"#),
                calls: AtomicUsize::new(0),
                buffering: Notify::new(),
            };
            let feedback = Feedback::default();
            let result = feedback.complete(&model, &json!({}), &[]).await.unwrap();
            assert!(matches!(result, Some(Step::Reply { text, .. }) if text == "Recovered"));
            assert_eq!(model.calls.load(Ordering::Relaxed), 1);
            assert!(feedback.source.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_task_revision_change_cancels_the_buffered_retry() {
        let model = FallbackModel {
            streamed: None,
            buffered: None,
            calls: AtomicUsize::new(0),
            buffering: Notify::new(),
        };
        let feedback = Feedback::default();
        let input = json!({});
        let complete = feedback.complete(&model, &input, &[]);
        let change_revision = async {
            model.buffering.notified().await;
            feedback.stale.notify_one();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(complete, change_revision)
        })
        .await
        .unwrap();
        assert!(result.unwrap().is_none());
        assert_eq!(model.calls.load(Ordering::Relaxed), 1);
    }

    #[derive(Default)]
    struct TestChannel {
        typing: Notify,
        draft: Notify,
        sent: Mutex<Vec<(String, i32, String)>>,
    }

    #[async_trait]
    impl ChannelAdapter for TestChannel {
        fn name(&self) -> &'static str {
            "test"
        }

        async fn receive(&self, _: i64) -> Result<(Vec<Inbound>, i64)> {
            unreachable!()
        }

        async fn send(&self, _: &str, _: &str, _: Option<&str>) -> Result<Option<i64>> {
            unreachable!()
        }

        async fn typing(&self, user: &str) -> Result<Option<Duration>> {
            assert_eq!(user, "42");
            self.typing.notify_one();
            Ok(None)
        }

        async fn draft(&self, user: &str, id: i32, text: &str) -> Result<Option<Duration>> {
            self.sent
                .lock()
                .unwrap()
                .push((user.into(), id, text.into()));
            self.draft.notify_one();
            Ok(None)
        }
    }

    struct SlowReply<'a>(&'a TestChannel);

    #[async_trait]
    impl ModelProvider for SlowReply<'_> {
        async fn complete(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }

        async fn complete_streaming(
            &self,
            _: &str,
            _: &str,
            delta: &mut TextDelta<'_>,
        ) -> Result<String> {
            self.0.typing.notified().await;
            delta(r#"{"kind":"reply","text":"Partial answer"#);
            pending().await
        }
    }

    #[tokio::test]
    async fn types_and_streams_until_the_task_revision_changes() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "42").await.unwrap();
        sqlx::query("INSERT INTO tasks(id,user,title,updated) VALUES('draft-task','42','Draft',0)")
            .execute(&db)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        let feedback = Feedback::default();
        let channel = TestChannel::default();
        let model = SlowReply(&channel);
        let input = json!({});
        let revise = async {
            channel.draft.notified().await;
            sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
                .bind(&task.id)
                .execute(&db)
                .await
                .unwrap();
        };
        let ((), result, ()) = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(
                feedback.show(&db, &task, &channel),
                feedback.complete(&model, &input, &[]),
                revise,
            )
        })
        .await
        .unwrap();
        assert!(result.unwrap().is_none());
        let sent = channel.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "42");
        assert!(sent[0].1 > 0);
        assert_eq!(sent[0].2, "Partial answer");
    }
}
