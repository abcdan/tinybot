use super::{Outbound, Task};
use crate::security::{clip, now};
use anyhow::Result;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

pub(crate) const WAIT_PREFIX: &str = "ack-generate/";
pub(crate) const READY_PREFIX: &str = "ack-progress/";
const DELAY: i64 = 5;
const LEASE: i64 = 30;

// A progress notice only follows a Telegram message in the user's personal Chief chat.
const CHAT_SCOPE: &str = "t.chief_chat=1 AND t.agent IS NULL AND t.shared_memory_pair IS NULL
    AND t.schedule IS NULL AND u.suspended_at IS NULL AND t.state IN ('queued','running')";
const CANDIDATE_SCOPE: &str =
    "e.telegram_message_id>0 AND e.telegram_message_id=o.reply_to_message_id";
const CANDIDATE_JOIN: &str = "FROM outbox o JOIN tasks t ON t.id=o.task AND t.user=o.user
    JOIN users u ON u.id=t.user
    JOIN events e ON e.id=(SELECT max(id) FROM events WHERE task=t.id AND role='user')";

#[derive(FromRow)]
pub(crate) struct Generation {
    pub id: i64,
    pub task: String,
    pub user: String,
    pub revision: i64,
    pub request: String,
    pub queued: bool,
    event: i64,
    #[sqlx(default)]
    lease: i64,
}

pub(super) async fn schedule(
    tx: &mut Transaction<'_, Sqlite>,
    user: &str,
    task: &str,
    message: i64,
    already_working: bool,
) -> Result<()> {
    let revision: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT t.revision FROM tasks t JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND {CHAT_SCOPE}"
    ))
    .bind(task)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(revision) = revision else {
        return Ok(());
    };
    let button = format!("{WAIT_PREFIX}{revision}");
    if already_working {
        // Retarget an unsent notice to the latest input, leaving delivered notices alone.
        sqlx::query(
            "UPDATE outbox SET state='queued',body='',button=?,reply_to_message_id=?,
             retry_at=CASE WHEN state='queued' AND button LIKE 'ack-generate/%' THEN retry_at ELSE ? END
             WHERE id=(SELECT id FROM outbox WHERE task=? AND user=?
               AND state IN ('queued','generating')
               AND (button LIKE 'ack-generate/%' OR button LIKE 'ack-progress/%')
               ORDER BY id DESC LIMIT 1)",
        )
        .bind(button)
        .bind(message)
        .bind(now())
        .bind(task)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO outbox(user,task,body,button,reply_to_message_id,retry_at) VALUES(?,?,'',?,?,?)",
        )
        .bind(user)
        .bind(task)
        .bind(button)
        .bind(message)
        .bind(now() + DELAY)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(crate) async fn claim(db: &SqlitePool) -> Result<Option<Generation>> {
    let time = now();
    let due: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM outbox
         WHERE button LIKE 'ack-generate/%' AND state IN ('queued','generating') AND retry_at<=?)",
    )
    .bind(time)
    .fetch_one(db)
    .await?;
    if !due {
        return Ok(None);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    // A generation lease can be reclaimed after a worker or process stops.
    // Expired, superseded, and already acknowledged requests never reach the model.
    sqlx::query(&format!(
        "UPDATE outbox SET state='failed',body='',button=NULL
         WHERE button LIKE 'ack-generate/%' AND state IN ('queued','generating') AND retry_at<=?
           AND NOT EXISTS(SELECT 1 {CANDIDATE_JOIN}
             WHERE o.id=outbox.id AND {CHAT_SCOPE} AND {CANDIDATE_SCOPE}
               AND e.acknowledged=0 AND o.button='ack-generate/' || t.revision)"
    ))
    .bind(time)
    .execute(&mut *tx)
    .await?;
    let mut generation: Option<Generation> = sqlx::query_as(&format!(
        "SELECT o.id,t.id AS task,t.user,t.revision,e.body AS request,t.state='queued' AS queued,e.id AS event
         {CANDIDATE_JOIN} WHERE {CHAT_SCOPE} AND {CANDIDATE_SCOPE}
           AND e.acknowledged=0 AND o.button='ack-generate/' || t.revision
           AND o.state IN ('queued','generating') AND o.retry_at<=?
         ORDER BY o.retry_at,o.id LIMIT 1"
    ))
    .bind(time)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(generation) = &mut generation {
        generation.lease = time + LEASE;
        sqlx::query("UPDATE outbox SET state='generating',retry_at=? WHERE id=?")
            .bind(generation.lease)
            .bind(generation.id)
            .execute(&mut *tx)
            .await?;
        generation.request = clip(&generation.request, 800).to_owned();
    }
    tx.commit().await?;
    Ok(generation)
}

pub(crate) async fn finish(
    db: &SqlitePool,
    generation: &Generation,
    text: Option<&str>,
) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let button = format!("{WAIT_PREFIX}{}", generation.revision);
    let phase: Option<String> = sqlx::query_scalar(&format!(
        "SELECT t.state {CANDIDATE_JOIN}
         WHERE {CHAT_SCOPE} AND {CANDIDATE_SCOPE} AND o.id=? AND o.user=? AND o.task=?
           AND o.state='generating' AND o.retry_at=? AND o.button=? AND t.revision=?
           AND e.id=? AND e.acknowledged=0"
    ))
    .bind(generation.id)
    .bind(&generation.user)
    .bind(&generation.task)
    .bind(generation.lease)
    .bind(&button)
    .bind(generation.revision)
    .bind(generation.event)
    .fetch_optional(&mut *tx)
    .await?;
    let expected = if generation.queued {
        "queued"
    } else {
        "running"
    };
    let text = text.map(str::trim).filter(|text| !text.is_empty());
    if let (Some(phase), Some(text)) = (phase.as_deref(), text)
        && phase == expected
    {
        sqlx::query("UPDATE events SET acknowledged=1 WHERE id=?")
            .bind(generation.event)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE outbox SET state='queued',retry_at=?,body=?,button=? WHERE id=?")
            .bind(now())
            .bind(clip(text, 240))
            .bind(format!("{READY_PREFIX}{phase}/{}", generation.revision))
            .bind(generation.id)
            .execute(&mut *tx)
            .await?;
    } else if phase.as_deref() == Some("running") && generation.queued && text.is_some() {
        // The task left the queue while this message was being written.
        sqlx::query("UPDATE outbox SET state='queued',retry_at=? WHERE id=?")
            .bind(now())
            .bind(generation.id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "UPDATE outbox SET state='failed',body='',button=NULL
             WHERE id=? AND user=? AND task=? AND state='generating' AND retry_at=? AND button=?",
        )
        .bind(generation.id)
        .bind(&generation.user)
        .bind(&generation.task)
        .bind(generation.lease)
        .bind(button)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn ready(db: &SqlitePool, item: &Outbound) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 {CANDIDATE_JOIN}
         WHERE {CHAT_SCOPE} AND {CANDIDATE_SCOPE} AND o.id=? AND o.user=? AND o.task=?
           AND o.state='sending' AND o.button=? AND o.button='ack-progress/' || t.state || '/' || t.revision
           AND e.acknowledged=1
           AND NOT EXISTS(SELECT 1 FROM events h WHERE h.task=t.id AND h.role='handoff'
                          AND json_extract(h.body,'$.event')=e.id))"
    ))
    .bind(item.id)
    .bind(&item.user)
    .bind(&item.task)
    .bind(&item.button)
    .fetch_one(db)
    .await?)
}

pub(crate) async fn suppress(db: &SqlitePool, task: &Task) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let current = sqlx::query(
        "UPDATE events SET acknowledged=1 WHERE id=(SELECT max(e.id) FROM tasks t JOIN events e ON e.task=t.id
           WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND e.role='user')",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .execute(&mut *tx)
    .await?;
    if current.rows_affected() != 0 {
        sqlx::query(
            "UPDATE outbox SET state=CASE WHEN state='sending' THEN state ELSE 'skipped' END,
                 body='',button=NULL WHERE user=? AND task=?
             AND state IN ('queued','generating','sending') AND button IN (?,?,?)",
        )
        .bind(&task.user)
        .bind(&task.id)
        .bind(format!("{WAIT_PREFIX}{}", task.revision))
        .bind(format!("{READY_PREFIX}queued/{}", task.revision))
        .bind(format!("{READY_PREFIX}running/{}", task.revision))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
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
        store::seed(&db, "42").await.unwrap();
        sqlx::query("UPDATE users SET provider='codex' WHERE id='42'")
            .execute(&db)
            .await
            .unwrap();
        db
    }

    async fn message(db: &SqlitePool, message: i64, text: &str) {
        store::ingest_telegram(db, "42", message, "42", text, message)
            .await
            .unwrap();
    }

    async fn make_due(db: &SqlitePool) {
        sqlx::query("UPDATE outbox SET retry_at=0 WHERE button LIKE 'ack-generate/%'")
            .execute(db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fast_replies_precede_and_discard_delayed_generation() {
        let db = fixture().await;
        message(&db, 20, "How long is the day?").await;
        assert!(claim(&db).await.unwrap().is_none());
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        let task = store::claim(&db).await.unwrap().unwrap();
        store::settle(&db, &task, "idle", Some("24 hours."))
            .await
            .unwrap();
        make_due(&db).await;
        assert_eq!(
            store::claim_outbox(&db).await.unwrap().unwrap().body,
            "24 hours."
        );
        assert!(claim(&db).await.unwrap().is_none());
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn steering_replaces_unsent_generation_and_sending_is_revision_checked() {
        let db = fixture().await;
        message(&db, 20, "Compare the options.").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        make_due(&db).await;
        let old = claim(&db).await.unwrap().unwrap();
        assert_eq!(old.revision, task.revision);
        message(&db, 21, "Compare one more option.").await;
        finish(&db, &old, Some("Comparing the original options."))
            .await
            .unwrap();
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        let current = claim(&db).await.unwrap().unwrap();
        assert_eq!(current.id, old.id);
        assert_eq!(current.revision, old.revision + 1);
        assert_eq!(current.request, "Compare one more option.");
        finish(&db, &current, Some("Including the extra option."))
            .await
            .unwrap();
        let item = store::claim_outbox(&db).await.unwrap().unwrap();
        assert_eq!(item.reply_to_message_id, Some(21));
        assert!(ready(&db, &item).await.unwrap());
        message(&db, 22, "Include the shipping fee too.").await;
        assert!(!ready(&db, &item).await.unwrap());
    }

    #[tokio::test]
    async fn queued_progress_is_regenerated_after_claim_and_suppressed_by_a_draft() {
        let db = fixture().await;
        message(&db, 20, "Compare the options.").await;
        make_due(&db).await;
        let waiting = claim(&db).await.unwrap().unwrap();
        assert!(waiting.queued);
        let task = store::claim(&db).await.unwrap().unwrap();
        finish(&db, &waiting, Some("I've queued the comparison."))
            .await
            .unwrap();
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        let running = claim(&db).await.unwrap().unwrap();
        assert!(!running.queued);
        suppress(&db, &task).await.unwrap();
        finish(&db, &running, Some("I'm comparing the options."))
            .await
            .unwrap();
        assert!(store::claim_outbox(&db).await.unwrap().is_none());
        assert!(claim(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn draft_suppression_does_not_open_the_lane_while_progress_is_claimed() {
        let db = fixture().await;
        message(&db, 20, "Compare the options.").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        make_due(&db).await;
        let progress = claim(&db).await.unwrap().unwrap();
        finish(&db, &progress, Some("I'm comparing the options."))
            .await
            .unwrap();
        let item = store::claim_delivery(&db).await.unwrap().unwrap();
        assert!(ready(&db, &item).await.unwrap());
        suppress(&db, &task).await.unwrap();
        assert!(!ready(&db, &item).await.unwrap());
        store::settle_reply(&db, &task, "Comparison complete.", &[])
            .await
            .unwrap();
        assert!(store::claim_delivery(&db).await.unwrap().is_none());
        sqlx::query("UPDATE outbox SET state='skipped' WHERE id=?")
            .bind(item.id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            store::claim_delivery(&db).await.unwrap().unwrap().body,
            "Comparison complete."
        );
    }
}
