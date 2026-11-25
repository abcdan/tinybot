use crate::security::now;
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::{Acquire, FromRow, Sqlite, SqliteConnection, SqlitePool, Transaction};

const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;

fn week_start(at: i64) -> i64 {
    // The Unix epoch was a Thursday. Weeks here begin on Monday in UTC.
    (at + 3 * DAY).div_euclid(WEEK) * WEEK - 3 * DAY
}

#[derive(FromRow, Serialize)]
pub(crate) struct AlertUsage {
    week_starts_at: i64,
    weekly_cents: Option<i64>,
    spent_microusd: Option<i64>,
    model_calls: i64,
    cost_reported_calls: i64,
    notified: bool,
}

pub(crate) async fn alert_usage(
    connection: &mut SqliteConnection,
    user: &str,
) -> Result<AlertUsage> {
    current_usage(connection, user, now()).await
}

async fn current_usage(
    connection: &mut SqliteConnection,
    user: &str,
    time: i64,
) -> Result<AlertUsage> {
    let start = week_start(time);
    Ok(sqlx::query_as(
        "WITH calls AS (
            SELECT json_extract(e.detail,'$.usage.cost_usd') AS cost
            FROM audit_entries e JOIN audit_sessions s ON s.id=e.session
            JOIN tasks t ON t.id=s.task
            WHERE t.user=?1 AND e.at>=?2 AND e.at<=?3 AND e.kind='model_call'
         )
         SELECT ?2 AS week_starts_at,
                (SELECT weekly_cents FROM usage_alerts WHERE user=?1) AS weekly_cents,
                CAST(ROUND(SUM(cost)*1000000) AS INTEGER) AS spent_microusd,
                COUNT(*) AS model_calls,COUNT(cost) AS cost_reported_calls,
                coalesce((SELECT notified_week=?2 FROM usage_alerts WHERE user=?1),0) AS notified
         FROM calls",
    )
    .bind(user)
    .bind(start)
    .bind(time)
    .fetch_one(connection)
    .await?)
}

pub(crate) async fn set_alert(db: &SqlitePool, user: &str, cents: Option<i64>) -> Result<()> {
    ensure!(
        cents.is_none_or(|amount| (1..=1_000_000).contains(&amount)),
        "invalid weekly alert amount"
    );
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let change = sqlx::query(
        "INSERT INTO usage_alerts(user,weekly_cents)
         SELECT id,? FROM users WHERE id=? AND suspended_at IS NULL
         ON CONFLICT(user) DO UPDATE SET weekly_cents=excluded.weekly_cents",
    )
    .bind(cents)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    ensure!(change.rows_affected() == 1, "account unavailable");
    if cents.is_some() {
        evaluate(&mut tx, user, now()).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn record_cost(db: &SqlitePool, session: i64) -> Result<()> {
    let mut connection = db.acquire().await?;
    let Some(user) = alert_owner(&mut connection, session).await? else {
        return Ok(());
    };
    drop(connection);
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    evaluate(&mut tx, &user, now()).await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn record_ingress_cost(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
) -> Result<()> {
    let session: Option<i64> = sqlx::query_scalar("SELECT audit_session_id FROM tasks WHERE id=?")
        .bind(task)
        .fetch_one(&mut **tx)
        .await?;
    if let Some(session) = session
        && let Some(user) = alert_owner(tx, session).await?
    {
        let mut notice = tx.begin().await?;
        evaluate(&mut notice, &user, now()).await?;
        notice.commit().await?;
    }
    Ok(())
}

async fn alert_owner(connection: &mut SqliteConnection, session: i64) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT t.user FROM audit_sessions s JOIN tasks t ON t.id=s.task
         JOIN users u ON u.id=t.user JOIN usage_alerts b ON b.user=t.user
         WHERE s.id=? AND u.suspended_at IS NULL AND b.weekly_cents IS NOT NULL
           AND b.notified_week IS NOT ?",
    )
    .bind(session)
    .bind(week_start(now()))
    .fetch_optional(connection)
    .await?)
}

async fn evaluate(tx: &mut Transaction<'_, Sqlite>, user: &str, time: i64) -> Result<()> {
    let usage = current_usage(tx, user, time).await?;
    let (Some(limit), Some(spend)) = (usage.weekly_cents, usage.spent_microusd) else {
        return Ok(());
    };
    if usage.notified || spend < limit * 10_000 {
        return Ok(());
    }
    let claimed = sqlx::query(
        "UPDATE usage_alerts SET notified_week=? WHERE user=? AND weekly_cents=?
           AND notified_week IS NOT ? AND EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL)",
    )
    .bind(usage.week_starts_at)
    .bind(user)
    .bind(limit)
    .bind(usage.week_starts_at)
    .bind(user)
    .execute(&mut **tx)
    .await?;
    if claimed.rows_affected() == 1 {
        sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
            .bind(user)
            .bind(format!(
                "TinyBot reached your weekly spend alert of ${:.2}. Providers reported ${:.6} across {} of {} model calls this week. Tasks can continue.",
                limit as f64 / 100.0,
                spend as f64 / 1_000_000.0,
                usage.cost_reported_calls,
                usage.model_calls,
            ))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::week_start;

    #[test]
    fn reported_spend_window_starts_exactly_on_monday_utc() {
        let next_monday = 4 * 86_400;
        assert_eq!(week_start(0), -3 * 86_400);
        assert_eq!(week_start(next_monday - 1), -3 * 86_400);
        assert_eq!(week_start(next_monday), next_monday);
        assert_eq!(
            week_start(next_monday + 7 * 86_400),
            next_monday + 7 * 86_400
        );
    }
}
