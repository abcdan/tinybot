use super::{ENGINE, MAX_REPLY_BYTES, Step};
use crate::{
    channel::ChannelAdapter,
    model::{ModelProvider, parse_json_reply},
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
        let response = tokio::select! {
            _ = self.stale.notified() => return Ok(None),
            result = model.complete_streaming(ENGINE, &input, &mut append) => result?,
        };
        Ok(Some(parse_json_reply(&response)?))
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
    use super::reply_preview;

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
}
