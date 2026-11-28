use super::{ChannelAdapter, media};
use crate::{App, audit::ModelCall, model::MAX_VOICE_BYTES, store};
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::time::Duration;

#[derive(Deserialize)]
pub(super) struct Note {
    file_id: String,
    file_size: Option<u64>,
}

pub struct Received {
    external_id: i64,
    user: String,
    telegram_message_id: i64,
    note: Note,
    reply: Option<store::ReplyContext>,
}

impl Received {
    pub(super) fn new(
        external_id: i64,
        user: String,
        telegram_message_id: i64,
        note: Note,
        reply: Option<store::ReplyContext>,
    ) -> Self {
        Self {
            external_id,
            user,
            telegram_message_id,
            note,
            reply,
        }
    }
}

pub(super) async fn handle(
    app: &App,
    operator: &str,
    adapter: &dyn ChannelAdapter,
    received: &Received,
) -> Result<()> {
    if !eligible(&app.db, adapter.name(), received).await? {
        return Ok(());
    }
    if oversized(received.note.file_size) {
        return notify(
            &app.db,
            adapter.name(),
            received,
            "This voice note is too large. Send a note under 10 MB or type your message.",
            None,
        )
        .await;
    }
    let transcriber = match app.models.voice_transcriber(&received.user).await {
        Ok(Some(transcriber)) => transcriber,
        Ok(None) => {
            return notify(
                &app.db,
                adapter.name(),
                received,
                "To use voice notes, add your OpenRouter API key under Model settings.",
                Some("mini"),
            )
            .await;
        }
        Err(error) if error.downcast_ref::<sqlx::Error>().is_some() => return Err(error),
        Err(_) => {
            return notify(
                &app.db,
                adapter.name(),
                received,
                "The OpenRouter connection couldn't be read. Reconnect your key under Model settings.",
                Some("mini"),
            )
            .await;
        }
    };
    let _ = adapter.typing(&received.user).await;
    let result = tokio::time::timeout(Duration::from_secs(35), async {
        let audio = adapter
            .download_file(&received.note.file_id, MAX_VOICE_BYTES)
            .await?;
        transcriber.transcribe(&audio).await
    })
    .await;
    match result {
        Ok(Ok(transcript)) => {
            enqueue_transcript(
                &app.db,
                operator,
                received,
                &transcript.text,
                (&transcript.usage, transcript.duration_ms),
            )
            .await
        }
        _ => notify(
            &app.db, adapter.name(), received,
            "I couldn't transcribe that voice note. Try recording it again or send your message as text.",
            None,
        ).await,
    }
}

async fn eligible(db: &SqlitePool, channel: &str, received: &Received) -> Result<bool> {
    media::eligible(
        db,
        channel,
        &received.user,
        received.external_id,
        received.telegram_message_id,
    )
    .await
}

async fn enqueue_transcript(
    db: &SqlitePool,
    operator: &str,
    received: &Received,
    transcript: &str,
    usage: (&ModelCall, u64),
) -> Result<()> {
    store::ingest_telegram_voice(
        db,
        operator,
        received.external_id,
        &received.user,
        &format!("Voice message:\n{transcript}"),
        received.telegram_message_id,
        received.reply.clone(),
        Some(usage),
    )
    .await
}

async fn notify(
    db: &SqlitePool,
    channel: &str,
    received: &Received,
    body: &str,
    button: Option<&str>,
) -> Result<()> {
    media::notify(
        db,
        channel,
        &received.user,
        received.external_id,
        body,
        button,
        Some(received.telegram_message_id),
    )
    .await
}

fn oversized(size: Option<u64>) -> bool {
    size.is_some_and(|size| size > MAX_VOICE_BYTES as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Inbound, Telegram};
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

    fn received(external_id: i64, user: &str) -> Received {
        Received::new(
            external_id,
            user.into(),
            51,
            Note {
                file_id: "voice-file".into(),
                file_size: Some(1250),
            },
            None,
        )
    }

    #[tokio::test]
    async fn voice_updates_are_collected_only_from_the_senders_private_chat() {
        let updates = json!({"ok":true,"result":[
            {"update_id":10,"message":{"message_id":51,"chat":{"id":42,"type":"private"},
                "from":{"id":42,"is_bot":false},"voice":{"file_id":"recording","file_size":1250},
                "reply_to_message":{"message_id":50,"chat":{"id":42,"type":"private"},
                    "from":{"id":99,"is_bot":true},"text":"The earlier answer."}}},
            {"update_id":11,"message":{"message_id":52,"chat":{"id":-99,"type":"group"},
                "from":{"id":42,"is_bot":false},"voice":{"file_id":"group-recording"}}},
            {"update_id":12,"message":{"message_id":53,"chat":{"id":43,"type":"private"},
                "from":{"id":42,"is_bot":false},"voice":{"file_id":"other-recording"}}}
        ]});
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let telegram = Telegram {
            client: reqwest::Client::new(),
            api: format!("http://{}/botTEST", listener.local_addr().unwrap()),
            origin: "https://example.test".into(),
            identity: Default::default(),
        };
        let fixture = Router::new().route(
            "/botTEST/getUpdates",
            post(move || async move { Json(updates) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
        let (events, offset) = telegram.receive(0).await.unwrap();
        assert_eq!(offset, 13);
        let [Inbound::Voice(received)] = events.as_slice() else {
            panic!("expected a single private voice message");
        };
        assert_eq!(received.external_id, 10);
        assert_eq!(received.telegram_message_id, 51);
        assert_eq!(received.user, "42");
        assert_eq!(received.note.file_id, "recording");
        let reply = received.reply.as_ref().unwrap();
        assert_eq!(reply.message_id, 50);
        assert!(reply.from_bot);
        assert_eq!(reply.excerpt.as_deref(), Some("The earlier answer."));
        server.abort();
    }

    #[tokio::test]
    async fn voice_transcript_is_a_single_literal_telegram_message() {
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
        let mut note = received(10, "42");
        note.reply = Some(store::ReplyContext {
            message_id: 50,
            excerpt: Some("A spoken reply's context".into()),
            from_bot: true,
        });
        assert!(eligible(&db, "telegram", &note).await.unwrap());
        assert!(
            !eligible(&db, "telegram", &received(11, "unknown"))
                .await
                .unwrap()
        );
        let mut usage = ModelCall::new("openrouter", "openai/whisper-large-v3");
        usage.openrouter(&json!({
            "text": "Private transcript marker",
            "input_audio": {"data":"Private audio marker"},
            "usage": {"input_tokens": 15, "output_tokens": 5, "total_tokens": 20, "cost": 0.001}
        }));
        for _ in 0..2 {
            enqueue_transcript(&db, "42", &note, "/cancel", (&usage, 120))
                .await
                .unwrap();
        }
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        assert!(!eligible(&db, "telegram", &note).await.unwrap());
        let task = store::claim(&db).await.unwrap().unwrap();
        let events: Vec<(String, String, Option<i64>)> =
            sqlx::query_as("SELECT role,body,telegram_message_id FROM events WHERE task=?")
                .bind(&task.id)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            events,
            vec![("user".into(), "Voice message:\n/cancel".into(), Some(51))]
        );
        let reply = store::latest_reply_context(&db, &task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.message_id, 50);
        assert_eq!(reply.excerpt.as_deref(), Some("A spoken reply's context"));
        let usage_events: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT e.attempt,e.name,e.status,e.detail FROM audit_entries e
             JOIN tasks t ON t.audit_session_id=e.session WHERE t.id=? AND e.kind='model_call'",
        )
        .bind(&task.id)
        .fetch_all(&db)
        .await
        .unwrap();
        let [(attempt, name, status, detail)] = usage_events.as_slice() else {
            panic!("expected one usage entry in the voice note session");
        };
        assert_eq!(
            (*attempt, name.as_str(), status.as_str()),
            (0, "voice_transcription", "completed")
        );
        let detail: serde_json::Value = serde_json::from_str(detail).unwrap();
        assert_eq!(
            detail,
            json!({
                "provider":"openrouter",
                "model":"openai/whisper-large-v3",
                "duration_ms":120,
                "usage":{"input_tokens":15,"output_tokens":5,"total_tokens":20,"cost_usd":0.001}
            })
        );

        let mut failed = received(11, "42");
        failed.telegram_message_id = 52;
        for _ in 0..2 {
            notify(&db, "telegram", &failed, "Connect OpenRouter", Some("mini"))
                .await
                .unwrap();
        }
        let item = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(item.body, "Connect OpenRouter");
        assert_eq!(item.button.as_deref(), Some("mini"));
        assert_eq!(item.reply_to_message_id, Some(52));
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        assert!(!eligible(&db, "telegram", &failed).await.unwrap());
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_sessions")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(sessions, 1);
    }

    #[test]
    fn voice_download_is_confined_to_telegram_file_paths() {
        let api = "https://api.telegram.org/bot123:synthetic";
        let url = media::media_url(api, "voice/file_9.oga").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.telegram.org/file/bot123:synthetic/voice/file_9.oga"
        );
        for path in [
            "https://other.test/file",
            "/voice/file.oga",
            "voice/../file.oga",
            "voice//file.oga",
            "voice/%2e%2e/file.oga",
            "voice/file.oga?query",
            "voice/file.oga#fragment",
            "voice\\file.oga",
        ] {
            assert!(media::media_url(api, path).is_err(), "{path}");
        }
        assert!(media::media_url("https://other.test/bot123:synthetic", "voice/file.oga").is_err());
        assert!(!oversized(Some(MAX_VOICE_BYTES as u64)));
        assert!(oversized(Some(1 + MAX_VOICE_BYTES as u64)));
    }
}
