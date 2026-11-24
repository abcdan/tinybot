use crate::{
    security::{clip, id, now},
    store::Task,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub kind: String,
    title: String,
    input: String,
    at: Option<String>,
    cron: Option<String>,
}

#[derive(Serialize, FromRow)]
pub struct Schedule {
    pub id: String,
    owner: String,
    pub title: String,
    #[serde(skip)]
    input: String,
    kind: String,
    cron: Option<String>,
    pub time_zone: String,
    pub next_run: Option<i64>,
    pub state: String,
    pub revision: i64,
    #[serde(skip)]
    steps: String,
    #[serde(skip)]
    source: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineStep {
    pub instruction: String,
    pub agent: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineRequest {
    pub title: String,
    pub input: String,
    #[serde(default)]
    pub steps: Vec<RoutineStep>,
    pub at: Option<String>,
    pub cron: Option<String>,
}

#[derive(Serialize)]
pub struct ScheduleDetails {
    #[serde(flatten)]
    schedule: Schedule,
    instructions: String,
    local_at: Option<String>,
    scheduled_at: Option<String>,
    steps: Vec<RoutineStep>,
    source: Option<String>,
}

#[derive(FromRow, Serialize)]
pub struct Run {
    task: String,
    title: String,
    state: String,
    updated: i64,
    started_at: Option<i64>,
    schedule_revision: Option<i64>,
    trigger: Option<String>,
}

#[derive(Serialize)]
pub struct Progress {
    pub current: Option<RoutineStep>,
    pub results: Vec<String>,
    pub total: usize,
    pub source: Option<String>,
}

pub async fn timezone(db: &SqlitePool, user: &str) -> Result<Tz> {
    let saved: Option<String> = sqlx::query_scalar("SELECT time_zone FROM users WHERE id=?")
        .bind(user)
        .fetch_one(db)
        .await?;
    let zone = saved
        .or_else(|| std::env::var("TINYBOT_TIMEZONE").ok())
        .unwrap_or_else(|| "UTC".into());
    Ok(zone.parse().context("invalid IANA time zone")?)
}

pub async fn set_zone(db: &SqlitePool, user: &str, value: &str) -> Result<()> {
    let zone: Tz = value
        .parse()
        .context("enter an IANA time zone, such as Europe/Amsterdam")?;
    sqlx::query("UPDATE users SET time_zone=? WHERE id=?")
        .bind(zone.name())
        .bind(user)
        .execute(db)
        .await?;
    Ok(())
}

pub fn local_iso(timestamp: i64, zone: Tz) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&zone)
        .to_rfc3339()
}

fn next(expression: &str, zone: Tz, after: i64) -> Result<i64> {
    ensure!(
        expression.len() <= 110 && expression.split_whitespace().count() == 5,
        "use a five-field cron expression"
    );
    let expression: Cron = expression.parse().context("invalid cron expression")?;
    let time = DateTime::from_timestamp(after, 0)
        .context("invalid schedule time")?
        .with_timezone(&zone);
    Ok(expression
        .find_next_occurrence(&time, false)
        .context("cron has no upcoming occurrence")?
        .timestamp())
}

pub fn validate(request: &Request, zone: Tz, time: i64) -> Result<i64> {
    ensure!(
        matches!(request.kind.as_str(), "reminder" | "task"),
        "choose reminder or task"
    );
    validate_text(&request.title, &request.input)?;
    match (&request.at, &request.cron) {
        (Some(value), None) => {
            let at = DateTime::parse_from_rfc3339(value)
                .context("use an RFC 3339 time with a UTC offset")?
                .timestamp();
            ensure!(at > time, "pick a time in the future");
            Ok(at)
        }
        (None, Some(expression)) => next(expression, zone, time),
        _ => anyhow::bail!("provide exactly one of at or cron"),
    }
}

fn validate_text(title: &str, input: &str) -> Result<()> {
    ensure!(
        !title.trim().is_empty()
            && title.len() <= 100
            && !input.trim().is_empty()
            && input.len() <= 2000,
        "provide a title and task or reminder text"
    );
    Ok(())
}

fn routine_time(request: &RoutineRequest, zone: Tz, time: i64) -> Result<Option<i64>> {
    validate_text(&request.title, &request.input)?;
    let next = match (&request.at, &request.cron) {
        (Some(value), None) => {
            let at = if let Ok(date) = DateTime::parse_from_rfc3339(value) {
                date.timestamp()
            } else {
                let local = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
                    .context("enter a valid date and time")?;
                zone.from_local_datetime(&local)
                    .single()
                    .context("that local time is ambiguous or does not exist in this time zone")?
                    .timestamp()
            };
            ensure!(at > time, "pick a time in the future");
            Some(at)
        }
        (None, Some(expression)) => Some(next(expression, zone, time)?),
        (None, None) => None,
        _ => anyhow::bail!("provide one timing rule"),
    };
    Ok(next)
}

async fn validate_steps(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    steps: &[RoutineStep],
) -> Result<()> {
    ensure!(steps.len() <= 6, "use at most six routine steps");
    for step in steps {
        ensure!(
            !step.instruction.trim().is_empty() && step.instruction.len() <= 600,
            "give each routine step an instruction up to 600 characters"
        );
        if let Some(agent) = &step.agent {
            let owned: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE id=? AND owner=?)")
                    .bind(agent)
                    .bind(user)
                    .fetch_one(&mut **tx)
                    .await?;
            ensure!(owned, "a routine step uses an unavailable helper");
        }
    }
    Ok(())
}

pub async fn create(db: &SqlitePool, user: &str, request: Request, time: i64) -> Result<Schedule> {
    let zone = timezone(db, user).await?;
    let next = validate(&request, zone, time)?;
    let row:Schedule=sqlx::query_as("INSERT INTO schedules(id,owner,title,input,kind,cron,time_zone,next_run)
        SELECT ?,?,?,?,?,?,?,? WHERE (SELECT count(*) FROM schedules WHERE owner=? AND state IN ('active','paused','manual'))<50
        AND EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL) RETURNING *")
        .bind(id()).bind(user).bind(request.title).bind(request.input).bind(request.kind).bind(request.cron)
        .bind(zone.name()).bind(next).bind(user).bind(user).fetch_optional(db).await?
        .context("active schedule limit reached or account unavailable")?;
    Ok(row)
}

pub async fn save_routine(
    db: &SqlitePool,
    user: &str,
    request: RoutineRequest,
    source: Option<&str>,
    time: i64,
    task_scope: Option<&Task>,
) -> Result<Schedule> {
    let zone = timezone(db, user).await?;
    let next = routine_time(&request, zone, time)?;
    let state = if next.is_some() { "active" } else { "manual" };
    ensure!(!request.steps.is_empty(), "add at least one routine step");
    validate_source(source, &request.steps)?;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, task_scope).await?;
    validate_steps(&mut tx, user, &request.steps).await?;
    let row = sqlx::query_as(
        "INSERT INTO schedules(id,owner,title,input,kind,cron,time_zone,next_run,state,steps,source)
         SELECT ?,?,?,?,'task',?,?,?,?,?,?
         WHERE (SELECT count(*) FROM schedules WHERE owner=? AND state IN ('active','paused','manual'))<50
         RETURNING *",
    )
    .bind(id())
    .bind(user)
    .bind(request.title)
    .bind(request.input)
    .bind(request.cron)
    .bind(zone.name())
    .bind(next)
    .bind(state)
    .bind(serde_json::to_string(&request.steps)?)
    .bind(source)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await?
    .context("active schedule limit reached")?;
    tx.commit().await?;
    Ok(row)
}

pub async fn list(db: &SqlitePool, user: &str) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as(
        "SELECT * FROM schedules WHERE owner=?
        ORDER BY state NOT IN ('active','paused','manual'), next_run ASC NULLS LAST, id LIMIT 60",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

async fn owned<'a>(
    executor: impl sqlx::Executor<'a, Database = Sqlite>,
    user: &str,
    id: &str,
) -> Result<Schedule> {
    sqlx::query_as("SELECT * FROM schedules WHERE id=? AND owner=?")
        .bind(id)
        .bind(user)
        .fetch_optional(executor)
        .await?
        .context("schedule unavailable")
}

pub async fn details(db: &SqlitePool, user: &str, id: &str) -> Result<ScheduleDetails> {
    let schedule = owned(db, user, id).await?;
    let zone = schedule.time_zone.parse::<Tz>()?;
    let at = schedule
        .next_run
        .and_then(|time| DateTime::from_timestamp(time, 0))
        .map(|time| time.with_timezone(&zone));
    Ok(ScheduleDetails {
        instructions: schedule.input.clone(),
        local_at: at.map(|time| time.format("%Y-%m-%dT%H:%M").to_string()),
        scheduled_at: at.map(|time| time.to_rfc3339()),
        steps: serde_json::from_str(&schedule.steps)?,
        source: schedule.source.clone(),
        schedule,
    })
}

pub async fn edit(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
    request: RoutineRequest,
    time: i64,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let schedule = owned(&mut *tx, user, id).await?;
    ensure!(
        revision == schedule.revision,
        "schedule changed; refresh and try again"
    );
    ensure!(
        matches!(schedule.state.as_str(), "active" | "paused" | "manual"),
        "editable schedule unavailable"
    );
    ensure!(
        schedule.kind == "task" || request.steps.is_empty(),
        "a reminder cannot contain routine steps"
    );
    validate_source(schedule.source.as_deref(), &request.steps)?;
    validate_steps(&mut tx, user, &request.steps).await?;
    let zone = schedule.time_zone.parse::<Tz>()?;
    let next = routine_time(&request, zone, time)?;
    let state = match (next, schedule.state.as_str()) {
        (None, _) => "manual",
        (Some(_), "paused") => "paused",
        (Some(_), _) => "active",
    };
    sqlx::query(
        "UPDATE schedules SET title=?,input=?,cron=?,next_run=?,state=?,steps=?,revision=revision+1
         WHERE id=? AND owner=?",
    )
    .bind(request.title)
    .bind(request.input)
    .bind(request.cron)
    .bind(next)
    .bind(state)
    .bind(serde_json::to_string(&request.steps)?)
    .bind(id)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn validate_source(source: Option<&str>, steps: &[RoutineStep]) -> Result<()> {
    if source == Some("inbox") {
        ensure!(
            !steps.is_empty() && steps.iter().all(|step| step.agent.is_none()),
            "use Chief steps for an inbox briefing"
        );
    }
    Ok(())
}

pub async fn cancel(db: &SqlitePool, user: &str, id: &str) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE schedules SET state='cancelled',next_run=NULL,revision=revision+1
        WHERE owner=? AND id=? AND state IN ('active','paused','manual')
        AND EXISTS(SELECT 1 FROM users WHERE id=schedules.owner AND suspended_at IS NULL)",
    )
    .bind(user)
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    ensure!(changed == 1, "schedule unavailable");
    Ok(())
}

pub async fn set_paused(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
    pause: bool,
    time: i64,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let schedule = owned(&mut *tx, user, id).await?;
    ensure!(
        revision == schedule.revision,
        "schedule changed; refresh and try again"
    );
    let next_run = match (pause, schedule.state.as_str(), &schedule.cron) {
        (true, "active", _) => schedule.next_run,
        (false, "paused", Some(cron)) => Some(next(cron, schedule.time_zone.parse()?, time)?),
        (false, "paused", None) => {
            ensure!(
                schedule.next_run.is_some_and(|at| at > time),
                "choose a future time before resuming this schedule"
            );
            schedule.next_run
        }
        _ => anyhow::bail!("schedule unavailable"),
    };
    sqlx::query(
        "UPDATE schedules SET state=?,next_run=?,revision=revision+1 WHERE id=? AND owner=?",
    )
    .bind(if pause { "paused" } else { "active" })
    .bind(next_run)
    .bind(id)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn run_now(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
    request_id: &str,
    time: i64,
    task_scope: Option<&Task>,
) -> Result<String> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, task_scope).await?;
    let schedule = owned(&mut *tx, user, id).await?;
    if let Some(task) =
        sqlx::query_scalar("SELECT task FROM schedule_runs WHERE schedule=? AND request_id=?")
            .bind(id)
            .bind(request_id)
            .fetch_optional(&mut *tx)
            .await?
    {
        tx.commit().await?;
        return Ok(task);
    }
    ensure!(
        revision == schedule.revision,
        "schedule changed; refresh and try again"
    );
    ensure!(
        matches!(
            schedule.state.as_str(),
            "active" | "paused" | "manual" | "done"
        ),
        "schedule unavailable"
    );
    let task = enqueue(&mut tx, &schedule, time, Some(request_id))
        .await?
        .context("a run is already in progress or this account is unavailable")?;
    tx.commit().await?;
    Ok(task)
}

async fn active_owner(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: Option<&Task>,
) -> Result<()> {
    let permitted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL
         AND (? IS NULL OR EXISTS(SELECT 1 FROM tasks t WHERE t.id=? AND t.user=users.id
             AND t.revision=? AND t.state='running' AND t.agent IS NULL AND t.shared_memory_pair IS NULL)))",
    )
    .bind(user)
    .bind(task.map(|task| &task.id))
    .bind(task.map(|task| &task.id))
    .bind(task.map(|task| task.revision))
    .fetch_one(&mut **tx)
    .await?;
    ensure!(permitted, "task or account changed");
    Ok(())
}

pub async fn history(db: &SqlitePool, user: &str, id: &str) -> Result<Vec<Run>> {
    owned(db, user, id).await?;
    Ok(sqlx::query_as(
        "SELECT t.id AS task,t.title,t.state,t.updated,r.started_at,r.schedule_revision,r.trigger
         FROM tasks t LEFT JOIN schedule_runs r ON r.task=t.id
         WHERE t.schedule=? AND t.user=? AND t.state<>'revoked'
           AND (t.agent IS NULL OR EXISTS(SELECT 1 FROM agents a WHERE a.id=t.agent AND
              (a.owner=t.user OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=t.user))))
         ORDER BY coalesce(r.started_at,t.updated) DESC,t.id DESC LIMIT 30",
    )
    .bind(id)
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn run(db: SqlitePool) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        if dispatch(&db, now()).await.is_err() {
            tracing::warn!("scheduled work dispatch failed");
        }
    }
}

pub async fn dispatch(db: &SqlitePool, time: i64) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<Schedule> = sqlx::query_as(
        "SELECT s.* FROM schedules s JOIN users u ON u.id=s.owner
         WHERE s.state='active' AND s.next_run<=? AND u.suspended_at IS NULL
         ORDER BY s.next_run LIMIT 30",
    )
    .bind(time)
    .fetch_all(&mut *tx)
    .await?;
    for schedule in rows {
        let next = schedule.cron.as_ref().and_then(|cron| {
            schedule
                .time_zone
                .parse::<Tz>()
                .ok()
                .and_then(|zone| next(cron, zone, time).ok())
        });
        sqlx::query("UPDATE schedules SET next_run=?,state=? WHERE id=?")
            .bind(next)
            .bind(if next.is_some() {
                "active"
            } else if schedule.cron.is_some() {
                "invalid"
            } else {
                "done"
            })
            .bind(&schedule.id)
            .execute(&mut *tx)
            .await?;
        enqueue(&mut tx, &schedule, time, None).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn enqueue(
    tx: &mut Transaction<'_, Sqlite>,
    schedule: &Schedule,
    time: i64,
    request: Option<&str>,
) -> Result<Option<String>> {
    let task = id();
    let created = sqlx::query(
        "INSERT INTO tasks(id,user,title,state,goal,updated,schedule)
         SELECT ?,id,?,CASE WHEN provider IS NULL THEN 'setup' ELSE 'queued' END,?,?,?
         FROM users WHERE id=? AND suspended_at IS NULL AND NOT EXISTS(
            SELECT 1 FROM tasks WHERE schedule=? AND state IN ('queued','running','waiting','setup','uncertain')
         )",
    )
    .bind(&task)
    .bind(&schedule.title)
    .bind(clip(&schedule.input, 1500))
    .bind(time)
    .bind(&schedule.id)
    .bind(&schedule.owner)
    .bind(&schedule.id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if created == 0 {
        return Ok(None);
    }
    let instruction = match (request.is_some(), schedule.kind.as_str()) {
        (true, "reminder") => "Send me this saved reminder now:",
        (true, _) => "Run this saved task now:",
        (false, "reminder") => {
            "My scheduled reminder is due now. Send me a reminder based on this request:"
        }
        (false, _) => "My scheduled task is due now. Carry out this request:",
    };
    sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user',?)")
        .bind(&task)
        .bind(format!("{instruction}\n\n{}", schedule.input))
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO schedule_runs(task,schedule,schedule_revision,started_at,trigger,request_id,steps,source)
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(&task)
    .bind(&schedule.id)
    .bind(schedule.revision)
    .bind(time)
    .bind(if request.is_some() { "manual" } else { "schedule" })
    .bind(request)
    .bind(&schedule.steps)
    .bind(&schedule.source)
    .execute(&mut **tx)
    .await?;
    crate::audit::open(
        tx,
        &task,
        if request.is_some() {
            "Manual schedule run"
        } else {
            "Scheduled task"
        },
    )
    .await?;
    Ok(Some(task))
}

pub async fn progress(db: &SqlitePool, task: &Task) -> Result<Option<Progress>> {
    let snapshot: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT r.steps,r.results,r.source FROM schedule_runs r JOIN tasks t ON t.id=r.task
         JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND u.suspended_at IS NULL
           AND t.agent IS NULL AND t.shared_memory_pair IS NULL AND r.steps<>'[]'",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(db)
    .await?;
    let Some((steps, results, source)) = snapshot else {
        return Ok(None);
    };
    let mut steps: Vec<RoutineStep> = serde_json::from_str(&steps)?;
    let results: Vec<String> = serde_json::from_str(&results)?;
    let total = steps.len();
    let current = (results.len() < total).then(|| steps.swap_remove(results.len()));
    Ok(Some(Progress {
        current,
        results,
        total,
        source,
    }))
}

pub async fn complete_step(db: &SqlitePool, task: &Task, index: i64, result: &str) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE schedule_runs SET results=json_insert(results,'$[#]',?)
         WHERE task=? AND json_array_length(results)=? AND json_array_length(steps)>?
         AND EXISTS(SELECT 1 FROM tasks t JOIN users u ON u.id=t.user
           WHERE t.id=schedule_runs.task AND t.user=? AND t.revision=? AND t.state='running'
             AND t.agent IS NULL AND t.shared_memory_pair IS NULL AND u.suspended_at IS NULL)",
    )
    .bind(clip(result, 1200))
    .bind(&task.id)
    .bind(index)
    .bind(index)
    .bind(&task.user)
    .bind(task.revision)
    .execute(db)
    .await?
    .rows_affected();
    ensure!(changed == 1, "routine step changed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_cron_uses_local_time_and_validates_the_shape() {
        let before = DateTime::parse_from_rfc3339("2026-10-23T10:00:00Z")
            .unwrap()
            .timestamp();
        let zone = chrono_tz::Europe::Amsterdam;
        assert_eq!(
            local_iso(next("0 9 * * 1-5", zone, before).unwrap(), zone),
            "2026-10-26T09:00:00+01:00"
        );
        assert!(next("not a schedule", zone, before).is_err());
        assert!(next("0 0 9 * * 1", zone, before).is_err());
    }

    #[tokio::test]
    async fn due_dispatch_queues_both_kinds_without_overlapping_or_replaying() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        crate::store::seed(&db, "2").await.unwrap();
        set_zone(&db, "1", "UTC").await.unwrap();
        let start = DateTime::parse_from_rfc3339("2026-11-22T10:00:00Z")
            .unwrap()
            .timestamp();
        let reminder = create(
            &db,
            "1",
            Request {
                kind: "reminder".into(),
                title: "One time".into(),
                input: "Check the report".into(),
                at: Some("2026-11-22T10:01:00Z".into()),
                cron: None,
            },
            start,
        )
        .await
        .unwrap();
        let recurring = create(
            &db,
            "1",
            Request {
                kind: "task".into(),
                title: "Recurring".into(),
                input: "Draft the report".into(),
                at: None,
                cron: Some("*/10 * * * *".into()),
            },
            start,
        )
        .await
        .unwrap();
        assert!(cancel(&db, "2", &recurring.id).await.is_err());
        let late = start + 1800;
        dispatch(&db, late).await.unwrap();
        dispatch(&db, late).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM outbox")
                .fetch_one(&db)
                .await
                .unwrap(),
            0
        );
        let prompts: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT tasks.schedule, tasks.user, tasks.state, events.body FROM tasks
            JOIN events ON events.task=tasks.id WHERE tasks.agent IS NULL AND events.role='user'
            ORDER BY tasks.title",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].0, reminder.id);
        assert_eq!(prompts[0].1, "1");
        assert_eq!(prompts[0].2, "setup");
        assert!(prompts[0].3.contains("scheduled reminder is due now"));
        assert!(prompts[0].3.ends_with("Check the report"));
        assert_eq!(prompts[1].0, recurring.id);
        assert!(prompts[1].3.contains("scheduled task is due now"));
        assert!(prompts[1].3.ends_with("Draft the report"));
        assert_eq!(list(&db, "1").await.unwrap()[0].next_run, Some(late + 600));
        dispatch(&db, late + 600).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tasks")
                .fetch_one(&db)
                .await
                .unwrap(),
            2
        );
        assert_eq!(list(&db, "1").await.unwrap()[0].next_run, Some(late + 1200));
        cancel(&db, "1", &recurring.id).await.unwrap();
    }

    #[tokio::test]
    async fn editing_and_pausing_keeps_an_existing_run_on_its_original_steps() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        crate::store::seed(&db, "2").await.unwrap();
        set_zone(&db, "1", "UTC").await.unwrap();
        sqlx::query("UPDATE users SET provider='openrouter' WHERE id='1'")
            .execute(&db)
            .await
            .unwrap();
        let time = now();
        let definition = |instruction: &str| RoutineRequest {
            title: "Weekly review".into(),
            input: "Review the project.".into(),
            at: None,
            cron: Some("0 9 * * 1".into()),
            steps: vec![RoutineStep {
                instruction: instruction.into(),
                agent: None,
            }],
        };
        let schedule = save_routine(&db, "1", definition("First version"), None, time, None)
            .await
            .unwrap();
        assert!(history(&db, "2", &schedule.id).await.is_err());
        assert!(details(&db, "2", &schedule.id).await.is_err());
        set_paused(&db, "1", &schedule.id, 0, true, time)
            .await
            .unwrap();
        assert!(
            set_paused(&db, "1", &schedule.id, 0, false, time)
                .await
                .is_err()
        );
        let task = run_now(&db, "1", &schedule.id, 1, "first", time, None)
            .await
            .unwrap();
        assert_eq!(
            run_now(&db, "1", &schedule.id, 1, "first", time, None)
                .await
                .unwrap(),
            task
        );
        assert!(
            run_now(&db, "1", &schedule.id, 1, "second", time, None)
                .await
                .is_err()
        );
        assert_eq!(
            details(&db, "1", &schedule.id)
                .await
                .unwrap()
                .schedule
                .next_run,
            schedule.next_run
        );
        edit(
            &db,
            "1",
            &schedule.id,
            1,
            definition("Second version"),
            time,
        )
        .await
        .unwrap();
        let queued = crate::store::claim(&db).await.unwrap().unwrap();
        assert_eq!(queued.id, task);
        let progress = progress(&db, &queued).await.unwrap().unwrap();
        assert_eq!(progress.current.unwrap().instruction, "First version");
        assert_eq!(
            history(&db, "1", &schedule.id).await.unwrap()[0].schedule_revision,
            Some(1)
        );
        assert!(complete_step(&db, &queued, 1, "Wrong step").await.is_err());
        complete_step(&db, &queued, 0, "Completed step")
            .await
            .unwrap();
        assert!(complete_step(&db, &queued, 0, "Duplicate").await.is_err());
        let saved = details(&db, "1", &schedule.id).await.unwrap();
        assert_eq!(saved.steps[0].instruction, "Second version");
        assert_eq!(saved.schedule.state, "paused");
        set_paused(&db, "1", &schedule.id, 2, false, time)
            .await
            .unwrap();
        assert_eq!(
            details(&db, "1", &schedule.id)
                .await
                .unwrap()
                .schedule
                .state,
            "active"
        );
    }
}
