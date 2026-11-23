use super::{ChannelAdapter, Telegram};
use crate::{App, model::MAX_VOICE_BYTES, net, store};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use std::time::Duration;
use url::Url;

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
}

impl Received {
    pub(super) fn new(
        external_id: i64,
        user: String,
        telegram_message_id: i64,
        note: Note,
    ) -> Self {
        Self {
            external_id,
            user,
            telegram_message_id,
            note,
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
        let audio = adapter.download_voice(&received.note.file_id).await?;
        transcriber.transcribe(&audio).await
    })
    .await;
    match result {
        Ok(Ok(text)) => enqueue_transcript(&app.db, operator, received, &text).await,
        _ => notify(
            &app.db, adapter.name(), received,
            "I couldn't transcribe that voice note. Try recording it again or send your message as text.",
            None,
        ).await,
    }
}

async fn eligible(db: &SqlitePool, channel: &str, received: &Received) -> Result<bool> {
    if received.telegram_message_id <= 0 {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?
        AND NOT EXISTS (SELECT 1 FROM updates WHERE channel=? AND external_id=?))",
    )
    .bind(&received.user)
    .bind(channel)
    .bind(received.external_id)
    .fetch_one(db)
    .await?)
}

async fn enqueue_transcript(
    db: &SqlitePool,
    operator: &str,
    received: &Received,
    transcript: &str,
) -> Result<()> {
    // The prefix makes the transcript literal user input even if it begins with /cancel.
    store::ingest_telegram(
        db,
        operator,
        received.external_id,
        &received.user,
        &format!("Voice message:\n{transcript}"),
        received.telegram_message_id,
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
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let added = sqlx::query(
        "INSERT OR IGNORE INTO updates(channel,external_id)
         SELECT ?,? FROM users WHERE id=?",
    )
    .bind(channel)
    .bind(received.external_id)
    .bind(&received.user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added == 1 {
        sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,?)")
            .bind(&received.user)
            .bind(body)
            .bind(button)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn oversized(size: Option<u64>) -> bool {
    size.is_some_and(|size| size > MAX_VOICE_BYTES as u64)
}

pub(super) async fn download(adapter: &Telegram, file_id: &str) -> Result<Vec<u8>> {
    ensure!(
        !file_id.is_empty() && file_id.len() <= 1024 && !file_id.chars().any(char::is_control),
        "invalid Telegram voice file ID"
    );
    let response = adapter.call("getFile", json!({"file_id":file_id})).await?;
    ensure!(
        response["ok"] == true,
        "Telegram did not expose the voice file"
    );
    let file = &response["result"];
    ensure!(
        !oversized(file["file_size"].as_u64()),
        "Telegram voice file exceeds the size limit"
    );
    let path = file["file_path"]
        .as_str()
        .context("Telegram voice file has no path")?;
    let response = adapter
        .client
        .get(media_url(&adapter.api, path)?)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Telegram voice download failed"))?;
    ensure!(
        response.status().is_success(),
        "Telegram rejected the voice download"
    );
    ensure!(
        !oversized(response.content_length()),
        "Telegram voice file exceeds the size limit"
    );
    net::body(response, MAX_VOICE_BYTES)
        .await
        .map_err(|_| anyhow::anyhow!("Telegram voice download failed"))
}

fn media_url(api: &str, path: &str) -> Result<Url> {
    let token = api
        .strip_prefix("https://api.telegram.org/bot")
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 150
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b))
        })
        .context("invalid Telegram media endpoint")?;
    ensure!(
        !path.is_empty()
            && path.len() <= 512
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "invalid Telegram media path"
    );
    Ok(Url::parse(&format!(
        "https://api.telegram.org/file/bot{token}/{path}"
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Inbound;
    use axum::{Json, Router, routing::post};
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
        )
    }

    #[tokio::test]
    async fn voice_updates_are_collected_only_from_the_senders_private_chat() {
        let updates = json!({"ok":true,"result":[
            {"update_id":10,"message":{"message_id":51,"chat":{"id":42,"type":"private"},
                "from":{"id":42,"is_bot":false},"voice":{"file_id":"recording","file_size":1250}}},
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
        let note = received(10, "42");
        assert!(eligible(&db, "telegram", &note).await.unwrap());
        assert!(
            !eligible(&db, "telegram", &received(11, "unknown"))
                .await
                .unwrap()
        );
        for _ in 0..2 {
            enqueue_transcript(&db, "42", &note, "/cancel")
                .await
                .unwrap();
        }
        assert!(!eligible(&db, "telegram", &note).await.unwrap());
        let task = store::claim(&db).await.unwrap().unwrap();
        let events: Vec<(String, String, Option<i64>)> =
            sqlx::query_as("SELECT role,body,telegram_message_id FROM events WHERE task=?")
                .bind(task.id)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            events,
            vec![("user".into(), "Voice message:\n/cancel".into(), Some(51))]
        );

        let failed = received(11, "42");
        for _ in 0..2 {
            notify(&db, "telegram", &failed, "Connect OpenRouter", Some("mini"))
                .await
                .unwrap();
        }
        let item = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(item.body, "Connect OpenRouter");
        assert_eq!(item.button.as_deref(), Some("mini"));
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        assert!(!eligible(&db, "telegram", &failed).await.unwrap());
    }

    #[test]
    fn voice_download_is_confined_to_telegram_file_paths() {
        let api = "https://api.telegram.org/bot123:synthetic";
        let url = media_url(api, "voice/file_9.oga").unwrap();
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
            assert!(media_url(api, path).is_err(), "{path}");
        }
        assert!(media_url("https://other.test/bot123:synthetic", "voice/file.oga").is_err());
        assert!(!oversized(Some(MAX_VOICE_BYTES as u64)));
        assert!(oversized(Some(1 + MAX_VOICE_BYTES as u64)));
    }
}
