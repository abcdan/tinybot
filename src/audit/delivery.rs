use super::{RETENTION, VISIBLE_SESSIONS};
use crate::{security::now, store::Task};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, Sqlite, SqliteConnection, SqlitePool, Transaction};

pub(crate) async fn draft_delivered(db: &SqlitePool, task: &Task) -> Result<()> {
    sqlx::query(
        "UPDATE audit_sessions SET first_draft_at_ms=CAST(unixepoch('subsec') * 1000 AS INTEGER)
         WHERE first_draft_at_ms IS NULL AND started_at>?
           AND id=(SELECT audit_session_id FROM tasks WHERE id=? AND user=? AND revision=?)",
    )
    .bind(now() - RETENTION)
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn handoff_queued(tx: &mut Transaction<'_, Sqlite>, outbox: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_deliveries(outbox,session,kind,queued_at_ms)
         SELECT o.id,s.id,'progress',CAST(unixepoch('subsec') * 1000 AS INTEGER)
         FROM outbox o JOIN tasks t ON t.id=o.task AND t.user=o.user
         JOIN audit_sessions s ON s.id=t.audit_session_id
         WHERE o.id=? AND s.started_at>?",
    )
    .bind(outbox)
    .bind(now() - RETENTION)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// A multipart result is confirmed only when every reply or reaction in the run
// has a corresponding accepted delivery. A draft or an approval prompt can
// count as first feedback, even if there is no final result yet.
const TIMINGS: &str = "
    receipts AS (
        SELECT d.session,MIN(d.finished_at_ms) FILTER (WHERE d.state='sent') AS first_delivery,
               MIN(d.queued_at_ms) FILTER (WHERE d.kind IN ('reply','reaction')) AS first_result_queued,
               MAX(d.finished_at_ms) FILTER (WHERE d.kind IN ('reply','reaction') AND d.state='sent') AS last_result,
               COUNT(*) FILTER (WHERE d.kind IN ('reply','reaction')) AS results,
               COUNT(*) FILTER (WHERE d.kind IN ('reply','reaction') AND d.state='sent') AS sent,
               COUNT(*) FILTER (WHERE d.kind IN ('reply','reaction') AND d.state='uncertain') AS uncertain,
               COUNT(*) FILTER (WHERE d.kind IN ('reply','reaction') AND d.state='failed') AS failed,
               COUNT(*) FILTER (WHERE d.kind IN ('reply','reaction') AND d.state='skipped') AS skipped
        FROM audit_deliveries d JOIN visible s ON s.id=d.session GROUP BY d.session
    ),
    timings AS (
        SELECT s.id,s.task,s.status,
               max(0,s.first_attempt_at_ms-s.received_at_ms) AS queue_wait_ms,
               max(0,coalesce(min(s.first_draft_at_ms,r.first_delivery),s.first_draft_at_ms,r.first_delivery)
                   -s.received_at_ms) AS first_feedback_ms,
               CASE WHEN r.results>0 AND r.results=r.sent
                    THEN max(0,r.last_result-s.received_at_ms) END AS result_delivery_ms,
               CASE WHEN r.results>0 AND r.results=r.sent
                    THEN max(0,r.last_result-r.first_result_queued) END AS telegram_delivery_ms,
               CASE WHEN coalesce(r.results,0)=0 THEN 'none'
                    WHEN r.results=r.sent THEN 'delivered'
                    WHEN r.uncertain>0 THEN 'uncertain'
                    WHEN r.failed>0 THEN 'failed'
                    WHEN r.skipped>0 THEN 'skipped'
                    ELSE 'pending' END AS result_status
        FROM visible s LEFT JOIN receipts r ON r.session=s.id
    )";

#[derive(FromRow, Serialize)]
struct Summary {
    runs: i64,
    completed_runs: i64,
    checked_off_tasks: i64,
    automatic_runs_finished: i64,
    clarification_runs: i64,
    connection_setup_runs: i64,
    provider_failure_runs: i64,
    delivered_runs: i64,
    failed_deliveries: i64,
    uncertain_deliveries: i64,
    measured_queue_waits: i64,
    measured_first_feedback: i64,
    measured_result_deliveries: i64,
    average_queue_wait_ms: Option<i64>,
    average_first_feedback_ms: Option<i64>,
    average_result_delivery_ms: Option<i64>,
}

pub(super) async fn summary(
    connection: &mut SqliteConnection,
    user: &str,
    cutoff: i64,
) -> Result<Value> {
    let sql = format!(
        "WITH visible AS (SELECT s.* {VISIBLE_SESSIONS}), {TIMINGS}
         SELECT COUNT(*) AS runs,COUNT(*) FILTER (WHERE status='completed') AS completed_runs,
                COUNT(*) FILTER (WHERE status='completed' AND EXISTS(SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='authorization' AND e.status='automatic') AND NOT EXISTS(SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='request' AND e.status='waiting')) AS automatic_runs_finished,
                COUNT(*) FILTER (WHERE EXISTS(SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='request' AND e.status='waiting' AND e.name IN ('form','elicitation','settings-form'))) AS clarification_runs,
                COUNT(*) FILTER (WHERE status='setup' OR EXISTS(SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='request' AND e.name IN ('connector-setup','mail-setup') AND e.status='waiting')) AS connection_setup_runs,
                COUNT(*) FILTER (WHERE EXISTS(SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='model' AND e.status='failed' AND json_extract(e.detail,'$.category') IS NOT NULL)) AS provider_failure_runs,
                COUNT(DISTINCT task) FILTER (WHERE EXISTS(
                    SELECT 1 FROM audit_entries e WHERE e.session=timings.id AND e.kind='task'
                      AND e.name='marked_complete' AND e.status='completed')) AS checked_off_tasks,
                COUNT(*) FILTER (WHERE result_status='delivered') AS delivered_runs,
                COUNT(*) FILTER (WHERE result_status='failed') AS failed_deliveries,
                COUNT(*) FILTER (WHERE result_status='uncertain') AS uncertain_deliveries,
                COUNT(queue_wait_ms) AS measured_queue_waits,
                COUNT(first_feedback_ms) AS measured_first_feedback,
                COUNT(result_delivery_ms) AS measured_result_deliveries,
                CAST(AVG(queue_wait_ms) AS INTEGER) AS average_queue_wait_ms,
                CAST(AVG(first_feedback_ms) AS INTEGER) AS average_first_feedback_ms,
                CAST(AVG(result_delivery_ms) AS INTEGER) AS average_result_delivery_ms
         FROM timings"
    );
    let summary: Summary = sqlx::query_as(&sql)
        .bind(user)
        .bind(cutoff)
        .fetch_one(connection)
        .await?;
    Ok(json!(summary))
}

#[derive(FromRow, Serialize)]
struct Timing {
    queue_wait_ms: Option<i64>,
    first_feedback_ms: Option<i64>,
    result_delivery_ms: Option<i64>,
    telegram_delivery_ms: Option<i64>,
    result_status: String,
}

#[derive(FromRow, Serialize)]
struct Receipt {
    kind: String,
    state: String,
    queued_at_ms: i64,
    finished_at_ms: Option<i64>,
    deferrals: i64,
}

/// Caller has already checked the run's visibility in this database snapshot.
pub(super) async fn session(connection: &mut SqliteConnection, id: i64) -> Result<Value> {
    let sql = format!(
        "WITH visible AS (SELECT * FROM audit_sessions s WHERE id=?), {TIMINGS}
         SELECT queue_wait_ms,first_feedback_ms,result_delivery_ms,telegram_delivery_ms,result_status FROM timings"
    );
    let timing: Timing = sqlx::query_as(&sql)
        .bind(id)
        .fetch_one(&mut *connection)
        .await?;
    let receipts: Vec<Receipt> = sqlx::query_as(
        "SELECT kind,state,queued_at_ms,finished_at_ms,deferrals
         FROM audit_deliveries WHERE session=? ORDER BY outbox",
    )
    .bind(id)
    .fetch_all(connection)
    .await?;
    // The provider-call records avoid double-counting nested model scopes.
    let (model_ms, review_ms): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT SUM(duration) FILTER (WHERE NOT is_review),SUM(duration) FILTER (WHERE is_review)
         FROM (
            SELECT json_extract(detail,'$.duration_ms') AS duration,
                   name IN ('action_review','mcp_read_decision','instruction_authorization') AS is_review
            FROM audit_entries
            WHERE session=? AND kind='model_call' AND status IN ('completed','failed')
              AND json_type(detail,'$.duration_ms')='integer'
         ) WHERE duration>=0",
    )
    .bind(id)
    .fetch_one(connection)
    .await?;
    Ok(json!({"timing":timing,"receipts":receipts,"provider_time":{
        "other_calls_ms":model_ms,"review_calls_ms":review_ms
    }}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audit::{self, Trace},
        store,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fixture() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        for user in ["1", "2"] {
            store::seed(&db, user).await.unwrap();
        }
        sqlx::query("UPDATE users SET provider='codex'")
            .execute(&db)
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn split_delivery_stays_with_the_original_run_after_a_follow_up() {
        let db = fixture().await;
        store::ingest_telegram(&db, "1", 1, "1", "Read the fixture page.", 12)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        let report = audit::sessions(&db, "1", None).await.unwrap();
        let first_run = report["sessions"][0]["id"].as_i64().unwrap();
        let trace = Trace::start(&db, &task).await.unwrap().unwrap();
        for (kind, name, status, detail) in [
            (
                "model_call",
                "agent_turn_stream",
                "completed",
                json!({"duration_ms":600}),
            ),
            (
                "model",
                "agent_turn_stream",
                "completed",
                json!({"duration_ms":700}),
            ),
            (
                "model_call",
                "action_review",
                "failed",
                json!({"duration_ms":200}),
            ),
            ("model_call", "agent_turn_stream", "started", Value::Null),
        ] {
            trace.push(kind, name, status, &detail).await.unwrap();
        }
        sqlx::query("UPDATE audit_sessions SET first_attempt_at_ms=received_at_ms+400 WHERE id=?")
            .bind(first_run)
            .execute(&db)
            .await
            .unwrap();
        draft_delivered(&db, &task).await.unwrap();
        store::settle_reply(
            &db,
            &task,
            &format!("{}\n\nFixture result", "long ".repeat(900)),
            &[],
        )
        .await
        .unwrap();
        store::ingest_telegram(&db, "1", 2, "1", "Check another source.", 13)
            .await
            .unwrap();
        let part = store::claim_outbox(&db).await.unwrap().unwrap();
        sqlx::query("UPDATE outbox SET state='queued',retry_at=0 WHERE id=?")
            .bind(part.id)
            .execute(&db)
            .await
            .unwrap();
        let resumed = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(resumed.id, part.id);
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(part.id)
            .execute(&db)
            .await
            .unwrap();
        let pending = audit::session(&db, "1", first_run, 0).await.unwrap();
        assert_eq!(pending["delivery"]["timing"]["result_status"], "pending");
        assert_eq!(
            pending["delivery"]["timing"]["result_delivery_ms"],
            Value::Null
        );
        assert_eq!(pending["delivery"]["timing"]["queue_wait_ms"], 400);
        assert_eq!(
            pending["delivery"]["timing"]["telegram_delivery_ms"],
            Value::Null
        );
        assert_eq!(
            pending["delivery"]["provider_time"],
            json!({"other_calls_ms":600,"review_calls_ms":200})
        );
        assert!(pending["delivery"]["timing"]["first_feedback_ms"].is_number());
        assert_eq!(pending["delivery"]["receipts"][1]["deferrals"], 1);

        let last = store::claim_outbox(&db).await.unwrap().unwrap();
        sqlx::query("UPDATE outbox SET state='sent' WHERE id=?")
            .bind(last.id)
            .execute(&db)
            .await
            .unwrap();
        let delivered = audit::export(&db, "1", first_run).await.unwrap();
        assert_eq!(
            delivered["delivery"]["timing"]["result_status"],
            "delivered"
        );
        assert!(delivered["delivery"]["timing"]["result_delivery_ms"].is_number());
        assert!(delivered["delivery"]["timing"]["telegram_delivery_ms"].is_number());
        let tally = audit::sessions(&db, "1", None).await.unwrap();
        assert_eq!(tally["delivery"]["runs"], 2);
        assert_eq!(tally["delivery"]["completed_runs"], 1);
        assert_eq!(tally["delivery"]["delivered_runs"], 1);
        assert_eq!(tally["delivery"]["checked_off_tasks"], 0);
        let latest = tally["sessions"][0]["id"].as_i64().unwrap();
        let latest = audit::session(&db, "1", latest, 0).await.unwrap();
        assert_eq!(latest["delivery"]["timing"]["result_status"], "none");
        assert!(audit::export(&db, "2", first_run).await.is_err());
        assert_eq!(
            audit::sessions(&db, "2", None).await.unwrap()["delivery"]["runs"],
            0
        );
    }

    #[tokio::test]
    async fn recovery_keeps_unconfirmed_delivery_distinct_from_a_completed_run() {
        let db = fixture().await;
        store::ingest_telegram(&db, "1", 1, "1", "Send a fixture result.", 12)
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        store::settle_reply(&db, &task, "Result", &[])
            .await
            .unwrap();
        let _ = store::claim_outbox(&db).await.unwrap().unwrap();
        store::recover(&db).await.unwrap();
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        let report = audit::sessions(&db, "1", None).await.unwrap();
        let run = report["sessions"][0]["id"].as_i64().unwrap();
        assert_eq!(report["delivery"]["completed_runs"], 1);
        assert_eq!(report["delivery"]["delivered_runs"], 0);
        assert_eq!(report["delivery"]["uncertain_deliveries"], 1);
        let delivery = &audit::export(&db, "1", run).await.unwrap()["delivery"];
        assert_eq!(delivery["timing"]["result_status"], "uncertain");
        assert_eq!(delivery["timing"]["result_delivery_ms"], Value::Null);
    }
}
