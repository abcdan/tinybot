use super::Task;
use crate::{
    audit,
    security::now,
    settings::{Key, Snapshot, Stale},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::collections::BTreeMap;

const RETENTION: i64 = 90 * 86_400;
const MAX_PER_USER: i64 = 200;
const PAGE_SIZE: usize = 20;

#[derive(Deserialize, Serialize)]
struct Delta {
    before: Option<String>,
    after: Option<String>,
}

type Changes = BTreeMap<Key, Delta>;

#[derive(sqlx::FromRow)]
struct Record {
    id: i64,
    created: i64,
    source: String,
    changes: String,
    restored_from: Option<i64>,
    latest: bool,
}

const RECORDS: &str =
    "SELECT h.*,NOT EXISTS(SELECT 1 FROM settings_history newer
        JOIN json_each(newer.changes) new_value JOIN json_each(h.changes) old_value
            ON new_value.key=old_value.key WHERE newer.user=h.user AND newer.id>h.id) AS latest
     FROM settings_history h JOIN users u ON u.id=h.user WHERE u.suspended_at IS NULL AND h.user=? AND h.created>=?";

pub(crate) async fn record<'a>(
    db: &mut SqliteConnection,
    user: &str,
    restored_from: Option<i64>,
    source: &str,
    changes: impl IntoIterator<Item = (Key, &'a str, &'a str)>,
) -> Result<()> {
    let changes: Changes = changes
        .into_iter()
        .filter(|(key, before, after)| key.history_eligible() && before != after)
        .map(|(key, before, after)| {
            (
                key,
                Delta {
                    before: audit::safe_history_text(before).map(str::to_owned),
                    after: audit::safe_history_text(after).map(str::to_owned),
                },
            )
        })
        .collect();
    if changes.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO settings_history(user,source,changes,restored_from,created) VALUES(?,?,?,?,?)",
    )
    .bind(user)
    .bind(source)
    .bind(serde_json::to_string(&changes)?)
    .bind(restored_from)
    .bind(now())
    .execute(&mut *db)
    .await?;
    sqlx::query(
        "DELETE FROM settings_history WHERE user=? AND id IN (
        SELECT id FROM settings_history WHERE user=? ORDER BY id DESC LIMIT -1 OFFSET ?)",
    )
    .bind(user)
    .bind(user)
    .bind(MAX_PER_USER)
    .execute(db)
    .await?;
    Ok(())
}

pub(super) async fn prune(db: &mut SqliteConnection) -> Result<()> {
    sqlx::query("DELETE FROM settings_history WHERE created<?")
        .bind(now() - RETENTION)
        .execute(db)
        .await?;
    Ok(())
}

pub(super) async fn expire_reviews(tx: &mut Transaction<'_, Sqlite>) -> Result<()> {
    let reviews: Vec<(String, i64)> = sqlx::query_as(
        "UPDATE pending SET state='expired',payload='{}' WHERE state='expiring' AND kind='settings-approve'
         AND EXISTS(SELECT 1 FROM settings_rollbacks r JOIN tasks t ON t.id=r.task
             WHERE t.id=pending.task AND t.revision=pending.revision AND t.state='waiting')
         RETURNING task,revision",
    )
    .fetch_all(&mut **tx)
    .await?;
    for (task, revision) in reviews {
        audit::request_changed(tx, &task, revision, "settings-approve", "expired").await?;
        close_review(tx, &task, revision).await?;
    }
    Ok(())
}

pub(crate) async fn list(db: &SqlitePool, user: &str, before: Option<i64>) -> Result<Value> {
    let mut tx = db.begin().await?;
    let rows: Vec<Record> = sqlx::query_as(&format!(
        "{RECORDS} AND (? IS NULL OR h.id<?) ORDER BY h.id DESC LIMIT ?"
    ))
    .bind(user)
    .bind(now() - RETENTION)
    .bind(before)
    .bind(before)
    .bind((PAGE_SIZE + 1) as i64)
    .fetch_all(&mut *tx)
    .await?;
    let current = crate::settings::snapshot(&mut tx, user).await?;
    tx.commit().await?;
    let next = (rows.len() > PAGE_SIZE).then(|| rows[PAGE_SIZE - 1].id);
    let mut items = Vec::new();
    for row in rows.into_iter().take(PAGE_SIZE) {
        let changes: Changes = serde_json::from_str(&row.changes)?;
        let can_restore = row.latest
            && changes.iter().all(|(&key, delta)| {
                key.history_eligible()
                    && delta.before.is_some()
                    && delta.after.as_deref() == Some(current.value(key))
            });
        let changes = changes
            .into_iter()
            .map(|(key, shift)| {
                json!({
                    "key":key,"label":key.label(),
                    "before":display(shift.before),"after":display(shift.after),
                })
            })
            .collect::<Vec<_>>();
        items.push(
            json!({"id":row.id,"created":row.created,"source":row.source,
            "restored_from":row.restored_from,"changes":changes,"can_restore":can_restore}),
        );
    }
    Ok(json!({"items":items,"next_before":next}))
}

fn display(value: Option<String>) -> String {
    match value.as_deref() {
        None => "Hidden".into(),
        Some("") => "Default".into(),
        Some(_) => value.unwrap_or_default(),
    }
}

pub(super) async fn restore_target(
    db: &mut SqliteConnection,
    user: &str,
    id: i64,
    current: &Snapshot,
) -> Result<BTreeMap<Key, String>> {
    let original: Record = sqlx::query_as(&format!("{RECORDS} AND h.id=?"))
        .bind(user)
        .bind(now() - RETENTION)
        .bind(id)
        .fetch_optional(db)
        .await?
        .context("Settings change unavailable.")?;
    ensure!(original.latest, Stale);
    let changes: Changes = serde_json::from_str(&original.changes)?;
    changes
        .into_iter()
        .map(|(key, shift)| {
            ensure!(key.history_eligible(), "Setting unavailable for restore.");
            let (Some(before), Some(after)) = (shift.before, shift.after) else {
                anyhow::bail!("This settings change contains hidden values.");
            };
            ensure!(current.value(key) == after, Stale);
            Ok((key, before))
        })
        .collect()
}

pub(super) async fn origin(db: &mut SqliteConnection, task: &str) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT change_id FROM settings_rollbacks WHERE task=?")
            .bind(task)
            .fetch_optional(db)
            .await?,
    )
}

pub(super) async fn close_review(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
    revision: i64,
) -> Result<()> {
    let task: Task = sqlx::query_as(
        "UPDATE tasks SET state='completed',updated=? WHERE id=? AND revision=? AND state='waiting' RETURNING *",
    )
    .bind(now()).bind(task).bind(revision)
    .fetch_one(&mut **tx).await?;
    audit::mark_task(tx, &task, "completed").await
}
