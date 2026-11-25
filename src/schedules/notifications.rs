use crate::{audit, security::now, store::Task};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

const ELIGIBLE: &str =
    "r.notify_changes=1 AND r.result_event IS NULL AND r.notification_state IS NULL
    AND json_array_length(r.results)>=json_array_length(r.steps)";

pub(crate) async fn previous(db: &SqlitePool, task: &Task) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT e.body FROM schedule_runs r JOIN tasks t ON t.id=r.task
         JOIN schedule_runs previous ON previous.schedule=r.schedule AND previous.schedule_revision=r.schedule_revision
           AND previous.task<>r.task AND previous.started_at<=r.started_at
         JOIN events e ON e.id=previous.result_event AND e.task=previous.task AND e.role='assistant'
         JOIN tasks prior_task ON prior_task.id=previous.task AND prior_task.user=t.user AND prior_task.state<>'revoked'
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND {ELIGIBLE}
           AND EXISTS(SELECT 1 FROM outbox o WHERE o.assistant_event_id=e.id AND o.state='sent')
           AND NOT EXISTS(SELECT 1 FROM outbox o WHERE o.assistant_event_id=e.id AND o.state<>'sent')
         ORDER BY previous.started_at DESC,e.id DESC LIMIT 1",
    ))
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(db)
    .await?)
}

pub(crate) async fn record_result(
    tx: &mut Transaction<'_, Sqlite>,
    task: &Task,
    event: i64,
) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE schedule_runs AS r SET result_event=? WHERE r.task=? AND {ELIGIBLE}"
    ))
    .bind(event)
    .bind(&task.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn unchanged(db: &SqlitePool, task: &Task) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query(&format!(
        "UPDATE tasks SET state='idle',updated=? WHERE id=? AND user=? AND revision=? AND state='running'
         AND EXISTS(SELECT 1 FROM schedule_runs r WHERE r.task=tasks.id AND {ELIGIBLE})
         AND EXISTS(SELECT 1 FROM users u WHERE u.id=tasks.user AND u.suspended_at IS NULL)",
    ))
    .bind(now())
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed != 0 {
        sqlx::query("UPDATE schedule_runs SET notification_state='unchanged' WHERE task=?")
            .bind(&task.id)
            .execute(&mut *tx)
            .await?;
        audit::mark_task(&mut tx, task, "idle").await?;
    }
    tx.commit().await?;
    Ok(changed != 0)
}
