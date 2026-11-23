use super::ingress::enqueue_input;
use crate::security::{id, now};
use anyhow::Result;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

const REPLY_TTL: i64 = 24 * 60 * 60;

#[derive(FromRow)]
pub struct SuggestedReply {
    pub token: String,
    pub body: String,
}

pub(super) async fn insert(
    tx: &mut Transaction<'_, Sqlite>,
    outbox: i64,
    revision: i64,
    suggestions: &[&str],
) -> Result<()> {
    let expires = now() + REPLY_TTL;
    for suggestion in suggestions {
        sqlx::query(
            "INSERT INTO suggested_replies(token,outbox,revision,body,expires) VALUES(?,?,?,?,?)",
        )
        .bind(id())
        .bind(outbox)
        .bind(revision)
        .bind(suggestion)
        .bind(expires)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub async fn suggested_replies(db: &SqlitePool, outbox: i64) -> Result<Vec<SuggestedReply>> {
    Ok(sqlx::query_as(
        "SELECT s.token,s.body FROM suggested_replies s
        JOIN outbox o ON o.id=s.outbox JOIN tasks t ON t.id=o.task
        WHERE s.outbox=? AND t.revision=s.revision AND t.state='idle' AND s.expires>=?
        ORDER BY s.rowid",
    )
    .bind(outbox)
    .bind(now())
    .fetch_all(db)
    .await?)
}

pub async fn redeem_reply(db: &SqlitePool, user: &str, token: &str) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let choice: Option<(i64, String, String, bool)> = sqlx::query_as(
        "SELECT o.id,t.id,s.body,u.provider IS NOT NULL FROM suggested_replies s
        JOIN outbox o ON o.id=s.outbox JOIN tasks t ON t.id=o.task JOIN users u ON u.id=t.user
        WHERE s.token=? AND o.user=? AND t.user=o.user AND s.expires>=?
        AND t.revision=s.revision AND t.state='idle'
        AND o.state IN ('sending','sent','uncertain')",
    )
    .bind(token)
    .bind(user)
    .bind(now())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((outbox, task, body, has_provider)) = choice else {
        return Ok(false);
    };
    if !enqueue_input(&mut tx, user, &task, &body, has_provider).await? {
        return Ok(false);
    }
    sqlx::query("UPDATE users SET active_task=? WHERE id=?")
        .bind(&task)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM suggested_replies WHERE outbox=?")
        .bind(outbox)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
