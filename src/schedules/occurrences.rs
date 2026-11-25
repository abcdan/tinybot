use super::{Schedule, active_owner, enqueue, next, owned};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

const DISPATCH_GRACE: i64 = 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    catch_up: String,
    quiet_start: Option<i64>,
    quiet_end: Option<i64>,
    #[serde(default)]
    notify_changes: bool,
}

pub(crate) async fn set_policy(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
    policy: Policy,
) -> Result<()> {
    ensure!(
        matches!(policy.catch_up.as_str(), "coalesce" | "skip"),
        "invalid catch-up policy"
    );
    match (policy.quiet_start, policy.quiet_end) {
        (None, None) => {}
        (Some(start), Some(end))
            if start != end && (0..1440).contains(&start) && (0..1440).contains(&end) => {}
        _ => anyhow::bail!("provide two different quiet-hour times"),
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let changed = sqlx::query(
        "UPDATE schedules SET catch_up=?,quiet_start=?,quiet_end=?,notify_changes=CASE WHEN kind='task' THEN ? ELSE 0 END,
           held_until=NULL,revision=revision+1
         WHERE id=? AND owner=? AND revision=? AND state IN ('active','paused','manual')",
    )
    .bind(policy.catch_up)
    .bind(policy.quiet_start)
    .bind(policy.quiet_end)
    .bind(policy.notify_changes)
    .bind(id)
    .bind(user)
    .bind(revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    ensure!(changed == 1, "schedule changed; refresh and try again");
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn snooze(
    db: &SqlitePool,
    user: &str,
    id: &str,
    revision: i64,
    minutes: i64,
    time: i64,
) -> Result<()> {
    ensure!(
        (0..=7 * 1440).contains(&minutes),
        "snooze for at most seven days"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    active_owner(&mut tx, user, None).await?;
    let until = (minutes > 0).then(|| time.saturating_add(minutes * 60));
    let changed = sqlx::query(
        "UPDATE schedules SET snoozed_until=?,held_until=NULL,revision=revision+1
         WHERE id=? AND owner=? AND revision=? AND state='active'",
    )
    .bind(until)
    .bind(id)
    .bind(user)
    .bind(revision)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    ensure!(changed == 1, "schedule changed; refresh and try again");
    tx.commit().await?;
    Ok(())
}

#[derive(FromRow, Serialize)]
pub(crate) struct Occurrence {
    due_at: i64,
    checked_at: i64,
    outcome: String,
    delayed_until: Option<i64>,
    task: Option<String>,
    task_state: Option<String>,
    notification_state: Option<String>,
    delivery: Option<String>,
}

pub(crate) async fn occurrences(db: &SqlitePool, user: &str, id: &str) -> Result<Vec<Occurrence>> {
    owned(db, user, id).await?;
    Ok(sqlx::query_as(
        "SELECT o.due_at,o.checked_at,
           CASE WHEN o.outcome='deferred' AND o.revision<>s.revision THEN 'superseded' ELSE o.outcome END AS outcome,
           o.delayed_until,t.id AS task,t.state AS task_state,r.notification_state,
           (SELECT b.state FROM outbox b WHERE b.task=t.id AND b.user=t.user
              AND b.assistant_event_id IS NOT NULL ORDER BY b.id DESC LIMIT 1) AS delivery
         FROM schedule_occurrences o JOIN schedules s ON s.id=o.schedule
         JOIN users u ON u.id=s.owner AND u.suspended_at IS NULL
         LEFT JOIN tasks t ON t.id=o.task AND t.user=s.owner AND t.state<>'revoked' AND t.agent IS NULL
         LEFT JOIN schedule_runs r ON r.task=t.id
         WHERE o.schedule=? AND s.owner=? ORDER BY o.due_at DESC,o.revision DESC LIMIT 30",
    )
    .bind(id)
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub(super) async fn dispatch(db: &SqlitePool, time: i64) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<Schedule> = sqlx::query_as(
        "SELECT s.* FROM schedules s JOIN users u ON u.id=s.owner
         WHERE s.state='active' AND s.next_run<=? AND (s.held_until IS NULL OR s.held_until<=?)
           AND u.suspended_at IS NULL ORDER BY s.next_run,s.id LIMIT 30",
    )
    .bind(time)
    .bind(time)
    .fetch_all(&mut *tx)
    .await?;
    for schedule in rows {
        dispatch_one(&mut tx, &schedule, time).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn dispatch_one(
    tx: &mut Transaction<'_, Sqlite>,
    schedule: &Schedule,
    time: i64,
) -> Result<()> {
    let due = schedule.next_run.context("due schedule has no time")?;
    let zone = schedule.time_zone.parse::<Tz>();
    let next_run = schedule.cron.as_deref().and_then(|cron| {
        zone.as_ref()
            .ok()
            .and_then(|zone| next(cron, *zone, time).ok())
    });
    let invalid = zone.is_err() || (schedule.cron.is_some() && next_run.is_none());
    if let Ok(zone) = zone
        && !invalid
    {
        let until = [
            schedule.snoozed_until.filter(|until| *until > time),
            quiet_until(schedule, zone, time),
        ]
        .into_iter()
        .flatten()
        .max();
        if let Some(until) = until {
            record(tx, schedule, due, time, "deferred", Some(until), None).await?;
            sqlx::query("UPDATE schedules SET held_until=? WHERE id=?")
                .bind(until)
                .bind(&schedule.id)
                .execute(&mut **tx)
                .await?;
            return Ok(());
        }
    }

    let skipped = schedule.cron.is_some()
        && schedule.catch_up == "skip"
        && time.saturating_sub(due) > DISPATCH_GRACE
        && schedule.held_until.is_none()
        && schedule.snoozed_until.is_none_or(|until| until <= due);
    let (outcome, task) = if invalid {
        ("invalid", None)
    } else if skipped {
        ("skipped_missed", None)
    } else if let Some(task) = enqueue(tx, schedule, time, None).await? {
        ("queued", Some(task))
    } else {
        ("skipped_overlap", None)
    };
    record(tx, schedule, due, time, outcome, None, task.as_deref()).await?;
    sqlx::query(
        "UPDATE schedules SET next_run=?,state=?,held_until=NULL,snoozed_until=NULL WHERE id=?",
    )
    .bind(next_run)
    .bind(if invalid {
        "invalid"
    } else if next_run.is_some() {
        "active"
    } else {
        "done"
    })
    .bind(&schedule.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn quiet_until(schedule: &Schedule, zone: Tz, time: i64) -> Option<i64> {
    let (start, end) = (schedule.quiet_start?, schedule.quiet_end?);
    let inside = |stamp| {
        DateTime::from_timestamp(stamp, 0).is_some_and(|date| {
            let local = date.with_timezone(&zone);
            let minute = i64::from(local.hour() * 60 + local.minute());
            if start < end {
                (start..end).contains(&minute)
            } else {
                minute >= start || minute < end
            }
        })
    };
    if !inside(time) {
        return None;
    }
    let minute = time - time.rem_euclid(60);
    (1..=26 * 60)
        .map(|offset| minute + offset * 60)
        .find(|stamp| !inside(*stamp))
}

async fn record(
    tx: &mut Transaction<'_, Sqlite>,
    schedule: &Schedule,
    due: i64,
    time: i64,
    outcome: &str,
    delay: Option<i64>,
    task: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO schedule_occurrences(schedule,revision,due_at,checked_at,outcome,delayed_until,task)
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(schedule,revision,due_at) DO UPDATE SET
           checked_at=excluded.checked_at,outcome=excluded.outcome,
           delayed_until=coalesce(excluded.delayed_until,schedule_occurrences.delayed_until),task=excluded.task
         WHERE schedule_occurrences.outcome='deferred'",
    )
    .bind(&schedule.id)
    .bind(schedule.revision)
    .bind(due)
    .bind(time)
    .bind(outcome)
    .bind(delay)
    .bind(task)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM schedule_occurrences WHERE schedule=? AND rowid IN (
           SELECT rowid FROM schedule_occurrences WHERE schedule=? ORDER BY due_at DESC,revision DESC
           LIMIT -1 OFFSET 200)",
    )
    .bind(&schedule.id)
    .bind(&schedule.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedules::{Request, create, set_zone};
    use serde_json::{Value, json};

    async fn fixture() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        set_zone(&db, "1", "Europe/Amsterdam").await.unwrap();
        db
    }

    fn timestamp(text: &str) -> i64 {
        DateTime::parse_from_rfc3339(text).unwrap().timestamp()
    }

    async fn receipts(db: &SqlitePool, schedule: &Schedule) -> Value {
        json!(occurrences(db, "1", &schedule.id).await.unwrap())
    }

    #[tokio::test]
    async fn quiet_hours_across_dst_defer_one_exact_reminder_without_a_model() {
        let db = fixture().await;
        let due = timestamp("2026-10-24T20:30:00Z");
        let morning = timestamp("2026-10-25T06:00:00Z");
        let schedule = create(
            &db,
            "1",
            Request {
                kind: "reminder".into(),
                title: "Pack".into(),
                input: "Pack the charger.".into(),
                at: Some("2026-10-24T20:30:00Z".into()),
                cron: None,
            },
            due - 120,
        )
        .await
        .unwrap();
        set_policy(
            &db,
            "1",
            &schedule.id,
            0,
            Policy {
                catch_up: "coalesce".into(),
                quiet_start: Some(22 * 60),
                quiet_end: Some(7 * 60),
                notify_changes: false,
            },
        )
        .await
        .unwrap();

        dispatch(&db, due).await.unwrap();
        dispatch(&db, morning - 1).await.unwrap();
        assert_eq!(receipts(&db, &schedule).await[0]["outcome"], "deferred");
        assert_eq!(receipts(&db, &schedule).await[0]["delayed_until"], morning);
        assert!(crate::store::claim_outbox(&db).await.unwrap().is_none());

        let (first, duplicate) = tokio::join!(dispatch(&db, morning), dispatch(&db, morning));
        first.unwrap();
        duplicate.unwrap();
        let receipt = receipts(&db, &schedule).await;
        assert_eq!(receipt.as_array().unwrap().len(), 1);
        assert_eq!(receipt[0]["delivery"], "queued");
        assert_eq!(receipt[0]["task_state"], "completed");
        assert_eq!(
            crate::store::claim_outbox(&db).await.unwrap().unwrap().body,
            "Pack the charger."
        );
        assert!(crate::store::claim_outbox(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_explicit_snooze_defers_a_due_run_even_under_skip_policy() {
        let db = fixture().await;
        let time = timestamp("2026-11-25T08:00:00Z");
        let schedule = create(
            &db,
            "1",
            Request {
                kind: "reminder".into(),
                title: "Review".into(),
                input: "Review the agenda.".into(),
                at: None,
                cron: Some("*/5 * * * *".into()),
            },
            time,
        )
        .await
        .unwrap();
        set_policy(
            &db,
            "1",
            &schedule.id,
            0,
            Policy {
                catch_up: "skip".into(),
                quiet_start: None,
                quiet_end: None,
                notify_changes: false,
            },
        )
        .await
        .unwrap();
        assert!(snooze(&db, "1", &schedule.id, 0, 20, time).await.is_err());
        snooze(&db, "1", &schedule.id, 1, 20, time).await.unwrap();
        dispatch(&db, time + 300).await.unwrap();
        assert_eq!(receipts(&db, &schedule).await[0]["outcome"], "deferred");
        dispatch(&db, time + 1200).await.unwrap();
        assert_eq!(receipts(&db, &schedule).await[0]["outcome"], "queued");
        dispatch(&db, time + 1800).await.unwrap();
        assert_eq!(
            receipts(&db, &schedule).await[0]["outcome"],
            "skipped_missed"
        );
    }
}
