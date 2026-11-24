use super::Telegram;
use crate::net;
use anyhow::{Context, Result, ensure};
use serde_json::json;
use sqlx::SqlitePool;
use url::Url;

pub(super) async fn eligible(
    db: &SqlitePool,
    channel: &str,
    user: &str,
    external_id: i64,
    message_id: i64,
) -> Result<bool> {
    if message_id <= 0 {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL
        AND NOT EXISTS (SELECT 1 FROM updates WHERE channel=? AND external_id=?))",
    )
    .bind(user)
    .bind(channel)
    .bind(external_id)
    .fetch_one(db)
    .await?)
}

pub(super) async fn notify(
    db: &SqlitePool,
    channel: &str,
    user: &str,
    external_id: i64,
    body: &str,
    button: Option<&str>,
    reply_to: Option<i64>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let added = sqlx::query(
        "INSERT OR IGNORE INTO updates(channel,external_id)
         SELECT ?,? FROM users WHERE id=?",
    )
    .bind(channel)
    .bind(external_id)
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added == 1 {
        sqlx::query("INSERT INTO outbox(user,body,button,reply_to_message_id) VALUES(?,?,?,?)")
            .bind(user)
            .bind(body)
            .bind(button)
            .bind(reply_to)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn download(adapter: &Telegram, file_id: &str, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        !file_id.is_empty() && file_id.len() <= 1024 && !file_id.chars().any(char::is_control),
        "invalid Telegram file ID"
    );
    let response = adapter.call("getFile", json!({"file_id":file_id})).await?;
    ensure!(response["ok"] == true, "Telegram did not expose the file");
    let file = &response["result"];
    ensure!(
        file["file_size"]
            .as_u64()
            .is_none_or(|size| size <= limit as u64),
        "Telegram file exceeds the size limit"
    );
    let path = file["file_path"]
        .as_str()
        .context("Telegram file has no path")?;
    let response = adapter
        .client
        .get(media_url(&adapter.api, path)?)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Telegram file download failed"))?;
    ensure!(
        response.status().is_success(),
        "Telegram rejected the file download"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        "Telegram file exceeds the size limit"
    );
    net::body(response, limit)
        .await
        .map_err(|_| anyhow::anyhow!("Telegram file download failed"))
}

pub(super) fn media_url(api: &str, path: &str) -> Result<Url> {
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
