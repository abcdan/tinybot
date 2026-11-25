use super::{RoutineStep, active_owner, owned};
use crate::{
    audit,
    security::{clip, id},
    store::Task,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::SqlitePool;

pub(crate) async fn queue(
    db: &SqlitePool,
    user: &str,
    schedule_id: &str,
    revision: i64,
    request: &str,
    sample: &str,
    time: i64,
) -> Result<String> {
    let sample = sample.trim();
    ensure!(
        !sample.is_empty() && sample.len() <= 2_000,
        "provide sample input up to 2,000 characters"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let schedule = owned(&mut *tx, user, schedule_id).await?;
    if let Some((task, preview)) = sqlx::query_as::<_, (String, bool)>(
        "SELECT task,preview_sample IS NOT NULL FROM schedule_runs WHERE schedule=? AND request_id=?",
    )
    .bind(schedule_id)
    .bind(request)
    .fetch_optional(&mut *tx)
    .await?
    {
        ensure!(preview, "run request already used");
        tx.commit().await?;
        return Ok(task);
    }
    ensure!(
        schedule.revision == revision,
        "schedule changed; refresh and try again"
    );
    ensure!(
        schedule.kind == "task"
            && matches!(schedule.state.as_str(), "manual" | "active" | "paused"),
        "routine unavailable for preview"
    );
    let task = id();
    let title = format!("Dry run · {}", schedule.title);
    let created = sqlx::query(
        "INSERT INTO tasks(id,user,title,state,goal,updated,schedule,chief_chat)
         SELECT ?,id,?,'queued',?,?,?,0 FROM users
         WHERE id=? AND provider IS NOT NULL AND NOT EXISTS(
            SELECT 1 FROM tasks t JOIN schedule_runs r ON r.task=t.id
            WHERE t.user=users.id AND r.preview_sample IS NOT NULL AND t.state IN ('queued','running')
         )",
    )
    .bind(&task)
    .bind(clip(&title, 120))
    .bind(&schedule.input)
    .bind(time)
    .bind(schedule_id)
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    ensure!(
        created == 1,
        "connect a model or wait for your current dry run to finish"
    );
    sqlx::query(
        "INSERT INTO schedule_runs(task,schedule,schedule_revision,started_at,trigger,request_id,steps,preview_sample)
         VALUES(?,?,?,?,'manual',?,?,?)",
    )
    .bind(&task)
    .bind(schedule_id)
    .bind(revision)
    .bind(time)
    .bind(request)
    .bind(&schedule.steps)
    .bind(sample)
    .execute(&mut *tx)
    .await?;
    audit::open(&mut tx, &task, "Routine dry run").await?;
    tx.commit().await?;
    Ok(task)
}

#[derive(Serialize)]
pub(crate) struct Preview {
    instructions: String,
    steps: Vec<String>,
    sample: String,
}

pub(crate) async fn context(db: &SqlitePool, task: &Task) -> Result<Option<Preview>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT t.goal,r.steps,r.preview_sample FROM schedule_runs r JOIN tasks t ON t.id=r.task
         JOIN users u ON u.id=t.user AND u.suspended_at IS NULL
         WHERE t.id=? AND t.user=? AND t.state='running' AND t.revision=? AND r.preview_sample IS NOT NULL",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(db)
    .await?;
    row.map(|(instructions, steps, sample)| {
        let steps: Vec<RoutineStep> = serde_json::from_str(&steps)?;
        Ok(Preview {
            instructions,
            steps: steps.into_iter().map(|step| step.instruction).collect(),
            sample,
        })
    })
    .transpose()
}
