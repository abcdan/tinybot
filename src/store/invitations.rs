use crate::security::{hash, id, now, token};
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

const LIFETIME: i64 = 24 * 60 * 60;
pub(crate) const BUTTON: &str = "bot-invitation";
pub(crate) const REVIEW: &str = "You’re invited to TinyBot. You’ll set up your own model account. Your conversations and memories stay private unless you explicitly publish a note to a shared space. The bot operator can see your account name, when you accept, whether your model is connected and whether you’ve received a first result. The operator also sees active days, counts for runs, delivered results and checked-off tasks, and provider-reported tokens and costs with coverage for the past seven days. Tap Accept invitation to continue.";

#[derive(Serialize)]
pub struct Invitation {
    pub id: String,
    pub command: String,
    pub expires: i64,
}

pub async fn create(tx: &mut Transaction<'_, Sqlite>, operator: &str) -> Result<Invitation> {
    let code = token();
    let id = id();
    let expires = now() + LIFETIME;
    sqlx::query("INSERT INTO bot_invitations(id,token_hash,issuer,expires) VALUES(?,?,?,?)")
        .bind(&id)
        .bind(hash(&code))
        .bind(operator)
        .bind(expires)
        .execute(&mut **tx)
        .await?;
    Ok(Invitation {
        id,
        command: format!("/start {code}"),
        expires,
    })
}

pub enum Review {
    Available(String),
    Unavailable,
    Other,
}

pub async fn review(db: &SqlitePool, operator: &str, user: &str, code: &str) -> Result<Review> {
    if code.len() != 43
        || !code
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_-".contains(&ch))
    {
        return Ok(Review::Other);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT id, expires>=? AND member IS NULL AND revoked_at IS NULL
           AND NOT EXISTS(SELECT 1 FROM users WHERE id=?)
         FROM bot_invitations WHERE token_hash=? AND issuer=?",
    )
    .bind(now())
    .bind(user)
    .bind(hash(code))
    .bind(operator)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((invitation, available)) = row else {
        return Ok(Review::Other);
    };
    if !available {
        return Ok(Review::Unavailable);
    }
    sqlx::query("INSERT OR IGNORE INTO bot_invitation_reviews VALUES(?,?)")
        .bind(&invitation)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Review::Available(invitation))
}

pub async fn shareable(db: &SqlitePool, issuer: &str, code: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM bot_invitations WHERE issuer=? AND token_hash=?
          AND member IS NULL AND revoked_at IS NULL AND expires>=?)",
    )
    .bind(issuer)
    .bind(hash(code))
    .bind(now())
    .fetch_one(db)
    .await?)
}

pub async fn accept(
    db: &SqlitePool,
    user: &str,
    invitation: &str,
    expanded_insights: bool,
) -> Result<bool> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let time = now();
    let created = sqlx::query(
        "INSERT OR IGNORE INTO users(id,host_id,onboarded_at)
         SELECT ?,?,? FROM bot_invitations i JOIN bot_invitation_reviews r ON r.invitation=i.id
         WHERE i.id=? AND i.expires>=? AND i.member IS NULL AND i.revoked_at IS NULL AND r.recipient=?",
    )
    .bind(user)
    .bind(format!("urn:uuid:{}", id()))
    .bind(time)
    .bind(invitation)
    .bind(time)
    .bind(user)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if created == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE bot_invitations SET member=?,accepted_at=?,insights_version=? WHERE id=?")
        .bind(user)
        .bind(time)
        .bind(if expanded_insights { 2 } else { 1 })
        .bind(invitation)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM bot_invitation_reviews WHERE invitation=?")
        .bind(invitation)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
        .bind(user)
        .bind("Invitation accepted. Open Settings & forms to set up your Chief and choose a model.")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn revoke(db: &SqlitePool, operator: &str, invitation: &str) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE bot_invitations SET revoked_at=? WHERE id=? AND issuer=?
         AND member IS NULL AND revoked_at IS NULL AND expires>=?",
    )
    .bind(now())
    .bind(invitation)
    .bind(operator)
    .bind(now())
    .execute(db)
    .await?;
    ensure!(changed.rows_affected() == 1, "invitation is unavailable");
    Ok(())
}

pub async fn available(db: &SqlitePool, user: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND suspended_at IS NULL)",
        )
        .bind(user)
        .fetch_one(db)
        .await?,
    )
}

pub async fn set_suspended(
    db: &SqlitePool,
    operator: &str,
    user: &str,
    suspended: bool,
) -> Result<()> {
    ensure!(operator != user, "cannot suspend the operator");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query("UPDATE users SET suspended_at=? WHERE id=?")
        .bind(suspended.then(now))
        .bind(user)
        .execute(&mut *tx)
        .await?;
    ensure!(changed.rows_affected() == 1, "guest is unavailable");
    if suspended {
        sqlx::query("UPDATE pending SET state='superseded' WHERE user=? AND state IN ('waiting','approved')")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE tasks SET state=CASE WHEN EXISTS
                    (SELECT 1 FROM pending p WHERE p.task=tasks.id AND p.state='executing')
                    THEN 'uncertain' ELSE 'idle' END,revision=revision+1 WHERE user=?
                    AND state IN ('running','waiting','queued','setup')",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE tasks SET state='revoked',revision=revision+1 WHERE state<>'revoked'
                    AND shared_memory_pair IN (SELECT id FROM shared_memory_pairs WHERE owner=? OR guest=?)")
            .bind(user)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE outbox SET state='failed',body='',button=NULL,widget=NULL WHERE state='queued' AND
                    (user=? OR task IN (SELECT t.id FROM tasks t JOIN shared_memory_pairs p ON p.id=t.shared_memory_pair
                    WHERE p.guest=? OR p.owner=?))")
            .bind(user)
            .bind(user)
            .bind(user)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(FromRow, Serialize)]
pub struct PendingInvite {
    id: String,
    expires: i64,
}

#[derive(FromRow, Serialize)]
pub struct Guest {
    id: String,
    display_name: Option<String>,
    accepted_at: Option<i64>,
    setup_complete: bool,
    suspended: bool,
    active_day: Option<i64>,
    runs_started: i64,
    runs_completed: i64,
    tokens: Option<i64>,
    cost_usd: Option<f64>,
    model_calls: i64,
    token_reported_calls: i64,
    cost_reported_calls: i64,
    active_days: Option<i64>,
    checked_off_tasks: Option<i64>,
    delivered_runs: Option<i64>,
    first_result: Option<bool>,
}

pub async fn pending(db: &SqlitePool, operator: &str) -> Result<Vec<PendingInvite>> {
    Ok(sqlx::query_as(
        "SELECT id,expires FROM bot_invitations WHERE issuer=? AND member IS NULL
                       AND revoked_at IS NULL AND expires>=? ORDER BY expires,id LIMIT 100",
    )
    .bind(operator)
    .bind(now())
    .fetch_all(db)
    .await?)
}

pub async fn guests(db: &SqlitePool, operator: &str) -> Result<Vec<Guest>> {
    Ok(sqlx::query_as(
        "WITH recent AS (
             SELECT s.id,t.id AS task,t.user,s.started_at,s.attempts,s.status FROM audit_sessions s
             JOIN tasks t ON t.id=s.task
             JOIN bot_invitations i ON i.member=t.user AND s.started_at>=i.accepted_at
             WHERE s.started_at>? AND i.issuer=?
         ), runs AS (
             SELECT user,max(started_at / 86400) * 86400 AS active_day,
                    count(DISTINCT started_at / 86400) AS active_days,
                    sum(attempts>0) AS runs_started, sum(status='completed') AS runs_completed
             FROM recent GROUP BY user
         ), results AS (
             SELECT r.user,COUNT(*) FILTER (WHERE delivered.session IS NOT NULL) AS delivered_runs,
                    COUNT(DISTINCT r.task) FILTER (WHERE EXISTS(SELECT 1 FROM audit_entries e
                        WHERE e.session=r.id AND e.kind='task' AND e.name='marked_complete'
                          AND e.status='completed')) AS checked_off_tasks
             FROM recent r LEFT JOIN account_delivered_runs delivered ON delivered.session=r.id
             GROUP BY r.user
         ), usage AS (
             SELECT r.user,count(*) AS model_calls,
               count(coalesce(json_extract(e.detail,'$.usage.total_tokens'),
                              json_extract(e.detail,'$.usage.input_tokens') + json_extract(e.detail,'$.usage.output_tokens'))) AS token_reported_calls,
               count(json_extract(e.detail,'$.usage.cost_usd')) AS cost_reported_calls,
               sum(coalesce(json_extract(e.detail,'$.usage.total_tokens'),
                            json_extract(e.detail,'$.usage.input_tokens') + json_extract(e.detail,'$.usage.output_tokens'))) AS tokens,
               sum(CAST(json_extract(e.detail,'$.usage.cost_usd') AS REAL)) AS cost_usd
             FROM audit_entries e JOIN recent r ON r.id=e.session WHERE e.kind='model_call' GROUP BY r.user
         )
         SELECT u.id,u.display_name,i.accepted_at,u.provider IS NOT NULL AS setup_complete,
                u.suspended_at IS NOT NULL AS suspended,r.active_day,
                coalesce(r.runs_started,0) AS runs_started,coalesce(r.runs_completed,0) AS runs_completed,
                usage.tokens,usage.cost_usd,
                coalesce(usage.model_calls,0) AS model_calls,
                coalesce(usage.token_reported_calls,0) AS token_reported_calls,
                coalesce(usage.cost_reported_calls,0) AS cost_reported_calls,
                CASE WHEN i.insights_version=2 THEN coalesce(r.active_days,0) END AS active_days,
                CASE WHEN i.insights_version=2 THEN coalesce(results.checked_off_tasks,0) END AS checked_off_tasks,
                CASE WHEN i.insights_version=2 THEN coalesce(results.delivered_runs,0) END AS delivered_runs,
                CASE WHEN i.insights_version=2 THEN u.first_result_at_ms IS NOT NULL END AS first_result
         FROM users u LEFT JOIN bot_invitations i ON i.member=u.id AND i.issuer=?2
         LEFT JOIN runs r ON r.user=u.id LEFT JOIN usage ON usage.user=u.id
         LEFT JOIN results ON results.user=u.id
         WHERE u.id<>?2 ORDER BY i.accepted_at DESC NULLS LAST,u.id LIMIT 200",
    )
    .bind(now() - crate::audit::RETENTION)
    .bind(operator)
    .fetch_all(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    async fn fixture() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "1").await.unwrap();
        db
    }

    async fn make_invite(db: &SqlitePool) -> String {
        let mut tx = db.begin().await.unwrap();
        let invitation = create(&mut tx, "1").await.unwrap();
        tx.commit().await.unwrap();
        invitation
            .command
            .strip_prefix("/start ")
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn enrollment_requires_review_and_single_use_consent() {
        let db = fixture().await;
        let code = make_invite(&db).await;
        let Review::Available(invitation) = review(&db, "1", "2", &code).await.unwrap() else {
            panic!("expected invitation review");
        };
        assert!(!available(&db, "2").await.unwrap());
        assert!(!accept(&db, "3", &invitation, false).await.unwrap());
        assert!(matches!(
            review(&db, "1", "3", &code).await.unwrap(),
            Review::Available(_)
        ));
        assert!(accept(&db, "2", &invitation, false).await.unwrap());
        assert!(!accept(&db, "3", &invitation, false).await.unwrap());
        assert!(available(&db, "2").await.unwrap());
        assert!(!available(&db, "3").await.unwrap());
        assert!(pending(&db, "1").await.unwrap().is_empty());
        assert!(guests(&db, "1").await.unwrap()[0].accepted_at.is_some());
        assert_eq!(
            store::claim_outbox(&db)
                .await
                .unwrap()
                .unwrap()
                .button
                .as_deref(),
            Some("mini")
        );

        let code = make_invite(&db).await;
        let Review::Available(revoked) = review(&db, "1", "4", &code).await.unwrap() else {
            panic!("expected invitation review");
        };
        revoke(&db, "1", &revoked).await.unwrap();
        assert!(!accept(&db, "4", &revoked, false).await.unwrap());
        assert!(matches!(
            review(&db, "1", "4", &code).await.unwrap(),
            Review::Unavailable
        ));
    }

    #[tokio::test]
    async fn suspending_blocks_existing_queued_work_and_metrics_return_only_aggregates() {
        let db = fixture().await;
        let code = make_invite(&db).await;
        let Review::Available(invite) = review(&db, "1", "2", &code).await.unwrap() else {
            panic!("expected invitation review");
        };
        assert!(accept(&db, "2", &invite, false).await.unwrap());
        sqlx::query("UPDATE users SET provider='codex' WHERE id='2'")
            .execute(&db)
            .await
            .unwrap();
        store::ingest(&db, "1", "test", 1, "2", "Private invitee request")
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        sqlx::query("UPDATE audit_sessions SET attempts=1,status='completed' WHERE task=?")
            .bind(&task.id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
           SELECT audit_session_id,1,?,'model_call','fixture','completed',? FROM tasks WHERE id=?",
        )
        .bind(now())
        .bind(r#"{"usage":{"total_tokens":15,"cost_usd":0},"private":"Do not return"}"#)
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
        store::seed(&db, "3").await.unwrap();
        store::ingest(&db, "1", "test", 2, "3", "Legacy activity")
            .await
            .unwrap();
        store::notify(&db, "2", "Private result", None)
            .await
            .unwrap();
        let outbox = store::claim_outbox(&db).await.unwrap().unwrap();
        set_suspended(&db, "1", "2", true).await.unwrap();
        assert!(!available(&db, "2").await.unwrap());
        assert!(!store::current(&db, &task).await.unwrap());
        assert!(!store::can_deliver(&db, &outbox).await.unwrap());
        let guests = guests(&db, "1").await.unwrap();
        assert_eq!((guests[0].runs_started, guests[0].runs_completed), (1, 1));
        assert_eq!(guests[0].tokens, Some(15));
        assert_eq!(guests[0].cost_usd, Some(0.0));
        assert_eq!(guests[1].id, "3");
        assert_eq!(guests[1].active_day, None);
        let payload = serde_json::to_string(&guests).unwrap();
        assert!(!payload.contains("Private invitee request") && !payload.contains("Do not return"));
        set_suspended(&db, "1", "2", false).await.unwrap();
        assert!(available(&db, "2").await.unwrap());
        assert!(!store::current(&db, &task).await.unwrap());
    }
}
