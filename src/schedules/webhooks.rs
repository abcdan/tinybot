use super::{Schedule, active_owner, enqueue, owned};
use crate::security::{Vault, id, token};
use anyhow::{Context, Result, ensure};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{FromRow, SqlitePool};

#[derive(FromRow)]
struct Hook {
    id: String,
    schedule: String,
    owner: String,
    schedule_revision: i64,
    signing_key: String,
}

#[derive(Serialize)]
pub(crate) struct Status {
    pub id: String,
    pub enabled: bool,
}

#[derive(Serialize)]
pub(crate) struct Installed {
    pub id: String,
    pub secret: String,
}

fn eligible(schedule: &Schedule) -> bool {
    schedule.kind == "task" && matches!(schedule.state.as_str(), "active" | "manual")
}

fn binding(id: &str, owner: &str, schedule: &str) -> String {
    format!("routine-webhook:{id}:{owner}:{schedule}")
}

pub(crate) async fn status(
    db: &SqlitePool,
    user: &str,
    schedule_id: &str,
) -> Result<Option<Status>> {
    let schedule = owned(db, user, schedule_id).await?;
    let hook: Option<(String, i64)> = sqlx::query_as(
        "SELECT id,schedule_revision FROM routine_webhooks WHERE schedule=? AND owner=?",
    )
    .bind(schedule_id)
    .bind(user)
    .fetch_optional(db)
    .await?;
    Ok(hook.map(|(id, revision)| Status {
        id,
        enabled: revision == schedule.revision && eligible(&schedule),
    }))
}

pub(crate) async fn install(
    db: &SqlitePool,
    vault: &Vault,
    user: &str,
    schedule_id: &str,
    revision: i64,
    time: i64,
) -> Result<Installed> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let schedule = owned(&mut *tx, user, schedule_id).await?;
    ensure!(
        schedule.revision == revision && eligible(&schedule),
        "routine changed or is unavailable"
    );
    let id = id();
    let secret = token();
    let sealed = vault.seal(&binding(&id, user, schedule_id), secret.as_bytes())?;
    sqlx::query(
        "INSERT INTO routine_webhooks(id,schedule,owner,schedule_revision,signing_key,created_at)
         VALUES(?,?,?,?,?,?) ON CONFLICT(schedule) DO UPDATE SET id=excluded.id,
           schedule_revision=excluded.schedule_revision,signing_key=excluded.signing_key,created_at=excluded.created_at
         WHERE routine_webhooks.owner=excluded.owner",
    )
    .bind(&id)
    .bind(schedule_id)
    .bind(user)
    .bind(revision)
    .bind(sealed)
    .bind(time)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Installed { id, secret })
}

pub(crate) async fn revoke(db: &SqlitePool, user: &str, schedule: &str, hook: &str) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM routine_webhooks WHERE id=? AND schedule=? AND owner=?")
        .bind(hook)
        .bind(schedule)
        .bind(user)
        .execute(db)
        .await?
        .rows_affected();
    ensure!(deleted == 1, "webhook changed or is unavailable");
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    event_id: String,
}

pub(crate) enum Delivery {
    Queued,
    Duplicate,
    Unauthorized,
    Busy,
    Invalid,
}

pub(crate) async fn receive(
    db: &SqlitePool,
    vault: &Vault,
    hook: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    time: i64,
) -> Result<Delivery> {
    if uuid::Uuid::parse_str(hook).is_err()
        || timestamp
            .parse::<i64>()
            .ok()
            .is_none_or(|value| value.abs_diff(time) > 300)
        || signature.len() != 64
        || body.len() > 2_048
    {
        return Ok(Delivery::Unauthorized);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let hook = sqlx::query_as::<_, Hook>("SELECT * FROM routine_webhooks WHERE id=?")
        .bind(hook)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(hook) = hook else {
        return Ok(Delivery::Unauthorized);
    };
    let key = vault.open(
        &binding(&hook.id, &hook.owner, &hook.schedule),
        &hook.signing_key,
    )?;
    let mut signer = Hmac::<Sha256>::new_from_slice(&key).context("invalid webhook key")?;
    signer.update(timestamp.as_bytes());
    signer.update(b".");
    signer.update(body);
    if hex::decode(signature)
        .ok()
        .is_none_or(|value| signer.verify_slice(&value).is_err())
    {
        return Ok(Delivery::Unauthorized);
    }
    let Ok(event) = serde_json::from_slice::<Event>(body) else {
        return Ok(Delivery::Invalid);
    };
    if event.event_id.is_empty()
        || event.event_id.len() > 128
        || !event
            .event_id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"-_.:".contains(&ch))
    {
        return Ok(Delivery::Invalid);
    }
    if active_owner(&mut tx, &hook.owner, None).await.is_err() {
        return Ok(Delivery::Unauthorized);
    }
    let schedule = owned(&mut *tx, &hook.owner, &hook.schedule).await?;
    if schedule.revision != hook.schedule_revision || !eligible(&schedule) {
        return Ok(Delivery::Unauthorized);
    }
    let request = format!("webhook/{}/{}", hook.id, event.event_id);
    let duplicate: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM schedule_runs WHERE schedule=? AND request_id=?)",
    )
    .bind(&hook.schedule)
    .bind(&request)
    .fetch_one(&mut *tx)
    .await?;
    if duplicate {
        tx.commit().await?;
        return Ok(Delivery::Duplicate);
    }
    let started = enqueue(&mut tx, &schedule, time, Some(&request))
        .await?
        .is_some();
    tx.commit().await?;
    Ok(if started {
        Delivery::Queued
    } else {
        Delivery::Busy
    })
}
