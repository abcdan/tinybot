use crate::security::{clip, id, now};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
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
    ensure!(
        !request.title.trim().is_empty()
            && request.title.len() <= 100
            && !request.input.trim().is_empty()
            && request.input.len() <= 2000,
        "provide a title and task or reminder text"
    );
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

pub async fn create(db: &SqlitePool, user: &str, request: Request, time: i64) -> Result<Schedule> {
    let zone = timezone(db, user).await?;
    let next = validate(&request, zone, time)?;
    let row:Schedule=sqlx::query_as("INSERT INTO schedules(id,owner,title,input,kind,cron,time_zone,next_run)
        SELECT ?,?,?,?,?,?,?,? WHERE (SELECT count(*) FROM schedules WHERE owner=? AND state='active')<50 RETURNING *")
        .bind(id()).bind(user).bind(request.title).bind(request.input).bind(request.kind).bind(request.cron)
        .bind(zone.name()).bind(next).bind(user).fetch_optional(db).await?.context("active schedule limit reached")?;
    Ok(row)
}

pub async fn list(db: &SqlitePool, user: &str) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as(
        "SELECT * FROM schedules WHERE owner=?
        ORDER BY state<>'active', next_run ASC NULLS LAST, id LIMIT 60",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn cancel(db: &SqlitePool, user: &str, id: &str) -> Result<()> {
    let changed=sqlx::query("UPDATE schedules SET state='cancelled',next_run=NULL WHERE owner=? AND id=? AND state='active'")
        .bind(user).bind(id).execute(db).await?.rows_affected();
    ensure!(changed == 1, "active schedule unavailable");
    Ok(())
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
        "SELECT * FROM schedules WHERE state='active' AND next_run<=? ORDER BY next_run LIMIT 30",
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
        let task = id();
        let created = sqlx::query(
            "INSERT INTO tasks(id,user,title,state,goal,updated,schedule)
            SELECT ?,id,?,CASE WHEN provider IS NULL THEN 'setup' ELSE 'queued' END,?,?,?
            FROM users WHERE id=? AND NOT EXISTS(
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
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if created == 1 {
            let instruction = if schedule.kind == "reminder" {
                "My scheduled reminder is due now. Send me a reminder based on this request:"
            } else {
                "My scheduled task is due now. Carry out this request:"
            };
            sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'user',?)")
                .bind(&task)
                .bind(format!("{instruction}\n\n{}", schedule.input))
                .execute(&mut *tx)
                .await?;
            crate::audit::open(&mut tx, &task, "Scheduled task").await?;
        }
    }
    tx.commit().await?;
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
}
