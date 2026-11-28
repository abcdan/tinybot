use crate::{audit, security::now, store::Task};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

const RESULT_PENDING: &str =
    "r.preview_sample IS NULL AND r.result_event IS NULL AND r.notification_state IS NULL
    AND json_array_length(r.results)>=json_array_length(r.steps)";

pub(crate) async fn previous(db: &SqlitePool, task: &Task) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT e.body FROM schedule_runs r JOIN tasks t ON t.id=r.task
         JOIN schedule_runs previous ON previous.schedule=r.schedule AND previous.schedule_revision=r.schedule_revision
           AND previous.task<>r.task AND previous.started_at<=r.started_at
         JOIN events e ON e.id=previous.result_event AND e.task=previous.task AND e.role='assistant'
         JOIN tasks prior_task ON prior_task.id=previous.task AND prior_task.user=t.user AND prior_task.state<>'revoked'
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND r.notify_changes=1 AND {RESULT_PENDING}
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
        "UPDATE schedule_runs AS r SET result_event=? WHERE r.task=? AND {RESULT_PENDING}
         AND (r.notify_changes=1 OR (r.trigger='manual' AND EXISTS(
           SELECT 1 FROM schedules s WHERE s.id=r.schedule AND s.notify_changes=1
             AND s.revision=r.schedule_revision)))"
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
         AND EXISTS(SELECT 1 FROM schedule_runs r WHERE r.task=tasks.id AND r.notify_changes=1 AND {RESULT_PENDING})
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{schedules, store};
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn manual_delivery_seeds_change_only_comparisons() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "1").await.unwrap();
        sqlx::query("UPDATE users SET provider='codex' WHERE id='1'")
            .execute(&db)
            .await
            .unwrap();
        let time = now();
        let routine = schedules::save_routine(
            &db,
            "1",
            schedules::RoutineRequest {
                title: "Release watch".into(),
                input: "Check the release date.".into(),
                steps: vec![schedules::RoutineStep {
                    instruction: "Read the release calendar.".into(),
                    agent: None,
                }],
                at: None,
                cron: Some("0 9 * * *".into()),
            },
            None,
            time,
            None,
        )
        .await
        .unwrap();
        let policy =
            serde_json::from_value(json!({"catch_up":"coalesce","notify_changes":true})).unwrap();
        schedules::set_policy(&db, "1", &routine.id, routine.revision, policy)
            .await
            .unwrap();

        let manual = schedules::run_now(
            &db,
            "1",
            &routine.id,
            routine.revision + 1,
            "manual-fixture",
            time,
            None,
        )
        .await
        .unwrap();
        let run = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(run.id, manual);
        schedules::complete_step(&db, &run, 0, "The date is Friday.")
            .await
            .unwrap();
        assert_eq!(previous(&db, &run).await.unwrap(), None);
        store::settle_reply(&db, &run, "The date is Friday.", &[])
            .await
            .unwrap();
        let message = store::claim_outbox(&db).await.unwrap().unwrap();

        schedules::dispatch(&db, routine.next_run.unwrap())
            .await
            .unwrap();
        let scheduled = store::claim(&db).await.unwrap().unwrap();
        schedules::complete_step(&db, &scheduled, 0, "The date is Friday.")
            .await
            .unwrap();
        assert_eq!(previous(&db, &scheduled).await.unwrap(), None);
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(message.id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            previous(&db, &scheduled).await.unwrap().as_deref(),
            Some("The date is Friday.")
        );
        assert!(unchanged(&db, &scheduled).await.unwrap());
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
    }
}
