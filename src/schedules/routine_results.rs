use super::owned;
use crate::{security::clip, store::Task};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate};
use chrono_tz::Tz;
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

const CANDIDATE_LIMIT: i64 = 20;

#[derive(FromRow, Serialize)]
pub(crate) struct Routine {
    id: String,
    title: String,
    time_zone: String,
}

#[derive(FromRow, Serialize)]
struct RunResult {
    task: String,
    started_at: i64,
    local_start: String,
    text: String,
    truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct RetainedResults {
    routines: Vec<Routine>,
    more_routines: bool,
    results: Vec<RunResult>,
}

pub(crate) struct Query<'a> {
    pub schedule: Option<&'a str>,
    pub title: Option<&'a str>,
    pub date: Option<NaiveDate>,
    pub limit: i64,
}

pub(crate) async fn read(
    db: &SqlitePool,
    task: &Task,
    query: Query<'_>,
) -> Result<RetainedResults> {
    let mut tx = db.begin().await?;
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tasks t JOIN users u ON u.id=t.user
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
           AND u.suspended_at IS NULL)",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        current,
        "This request changed before the routine results could be read."
    );

    let mut routines = match query.schedule {
        Some(id) => {
            let selected = owned(&mut *tx, &task.user, id).await?;
            ensure!(selected.kind == "task", "This schedule is a reminder.");
            vec![Routine {
                id: selected.id,
                title: selected.title,
                time_zone: selected.time_zone,
            }]
        }
        None => {
            sqlx::query_as::<_, Routine>(
                "SELECT s.id,s.title,s.time_zone FROM schedules s
             WHERE s.owner=?1 AND s.kind='task'
               AND (?2 IS NULL OR trim(s.title)=?2 COLLATE NOCASE)
               AND (?2 IS NOT NULL OR EXISTS(SELECT 1 FROM tasks t LEFT JOIN schedule_runs r ON r.task=t.id
                 WHERE t.schedule=s.id AND t.user=s.owner
                   AND t.state IN ('idle','completed') AND r.preview_sample IS NULL
                   AND t.chief_chat=1 AND t.agent IS NULL AND t.shared_memory_pair IS NULL
                   AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)))
             ORDER BY s.title COLLATE NOCASE,s.id LIMIT ?3",
            )
            .bind(&task.user)
            .bind(query.title)
            .bind(CANDIDATE_LIMIT + 1)
            .fetch_all(&mut *tx)
            .await?
        }
    };
    let more_routines = routines.len() > CANDIDATE_LIMIT as usize;
    routines.truncate(CANDIDATE_LIMIT as usize);
    let mut results: Vec<RunResult> = Vec::new();
    if let [routine] = routines.as_slice() {
        let zone = routine.time_zone.parse::<Tz>()?;
        // The UTC window covers every possible offset, then the local date check handles DST.
        let midnight = query
            .date
            .map(|date| date.and_time(chrono::NaiveTime::MIN).and_utc().timestamp());
        let mut rows = sqlx::query_as::<_, RunResult>(
            "SELECT t.id AS task,coalesce(r.started_at,t.created_at,t.updated) AS started_at,
                    '' AS local_start,e.body AS text,0 AS truncated
             FROM tasks t LEFT JOIN schedule_runs r ON r.task=t.id
             JOIN events e ON e.id=(
                 SELECT prior.id FROM events prior
                 WHERE prior.task=t.id AND prior.role='assistant' AND trim(prior.body)<>''
                   AND NOT EXISTS(SELECT 1 FROM events followup
                     WHERE followup.task=t.id AND followup.role='user'
                       AND followup.telegram_message_id>0 AND followup.id<prior.id)
                   AND EXISTS(SELECT 1 FROM outbox o
                     WHERE o.assistant_event_id=prior.id AND o.user=t.user AND o.task=t.id
                       AND o.state='sent')
                   AND NOT EXISTS(SELECT 1 FROM outbox o
                     WHERE o.assistant_event_id=prior.id AND o.state<>'sent')
                 ORDER BY prior.id DESC LIMIT 1
             )
             WHERE t.schedule=? AND t.user=? AND t.state IN ('idle','completed')
               AND t.chief_chat=1 AND t.agent IS NULL AND t.shared_memory_pair IS NULL
               AND NOT EXISTS(SELECT 1 FROM telegram_group_tasks g WHERE g.task=t.id)
               AND r.preview_sample IS NULL
               AND (?3 IS NULL OR coalesce(r.started_at,t.created_at,t.updated)>=?3
                                AND coalesce(r.started_at,t.created_at,t.updated)<?4)
             ORDER BY started_at DESC,e.id DESC LIMIT ?5",
        )
        .bind(&routine.id)
        .bind(&task.user)
        .bind(midnight.map(|time| time - 86_400))
        .bind(midnight.map(|time| time + 2 * 86_400))
        .bind(if query.date.is_some() {
            -1
        } else {
            query.limit
        })
        .fetch(&mut *tx);
        while results.len() < query.limit as usize {
            let Some(mut result) = rows.try_next().await? else {
                break;
            };
            let local_start = DateTime::from_timestamp(result.started_at, 0)
                .context("Invalid retained run time.")?
                .with_timezone(&zone);
            if query
                .date
                .is_none_or(|date| local_start.date_naive() == date)
            {
                result.local_start = local_start.to_rfc3339();
                results.push(result);
            }
        }
        let text_bytes = 4500 / results.len().max(1);
        for result in &mut results {
            result.text.retain(|character| {
                !character.is_control() || matches!(character, '\n' | '\r' | '\t')
            });
            result.truncated = result.text.len() > text_bytes;
            if result.truncated {
                result.text.truncate(clip(&result.text, text_bytes).len());
            }
        }
    }
    for routine in &mut routines {
        routine.title.truncate(clip(&routine.title, 120).len());
    }
    tx.commit().await?;
    Ok(RetainedResults {
        routines,
        more_routines,
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{schedules, store};

    fn recent(schedule: Option<&str>, limit: i64) -> Query<'_> {
        Query {
            schedule,
            title: None,
            date: None,
            limit,
        }
    }

    async fn add_routine(db: &SqlitePool, id: &str, user: &str, title: &str) {
        sqlx::query(
            "INSERT INTO schedules(id,owner,title,input,kind,time_zone,state)
             VALUES(?,?,?,'Write a digest','task','Europe/Amsterdam','manual')",
        )
        .bind(id)
        .bind(user)
        .bind(title)
        .execute(db)
        .await
        .unwrap();
    }

    async fn claim(db: &SqlitePool, expected: &str) -> Task {
        let task = store::claim(db).await.unwrap().unwrap();
        assert_eq!(task.id, expected);
        task
    }

    async fn send(db: &SqlitePool, task: &Task, text: &str, delivered: bool) {
        store::settle_reply(db, task, text, &[]).await.unwrap();
        if delivered {
            sqlx::query("UPDATE outbox SET state='sent' WHERE task=?")
                .bind(&task.id)
                .execute(db)
                .await
                .unwrap();
        }
    }

    async fn run_and_reply(
        db: &SqlitePool,
        user: &str,
        routine: &str,
        time: i64,
        text: &str,
        delivered: bool,
    ) -> Task {
        let task = schedules::run_now(db, user, routine, 0, &time.to_string(), time, None)
            .await
            .unwrap();
        let task = claim(db, &task).await;
        send(db, &task, text, delivered).await;
        task
    }

    #[tokio::test]
    async fn reads_only_delivered_initial_replies_from_owned_routines() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "42").await.unwrap();
        store::seed(&db, "43").await.unwrap();
        sqlx::query("UPDATE users SET provider='codex'")
            .execute(&db)
            .await
            .unwrap();
        add_routine(&db, "daily", "42", "Daily summary").await;
        add_routine(&db, "private", "43", "Private summary").await;
        let old = run_and_reply(&db, "42", "daily", 100, "Yesterday's summary", true).await;
        let latest = run_and_reply(&db, "42", "daily", 101, "Today's summary", true).await;
        run_and_reply(&db, "42", "daily", 102, "Unsent summary", false).await;
        run_and_reply(&db, "43", "private", 103, "Another user's summary", true).await;
        let preview = schedules::queue_preview(&db, "42", "daily", 0, "preview", "Sample", 104)
            .await
            .unwrap();
        send(&db, &claim(&db, &preview).await, "Preview summary", true).await;
        sqlx::query("UPDATE tasks SET chief_chat=1 WHERE id=?")
            .bind(&preview)
            .execute(&db)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO events(task,role,body,telegram_message_id) VALUES(?,'user','Follow-up',9)",
        )
        .bind(&latest.id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
            .bind(&latest.id)
            .execute(&db)
            .await
            .unwrap();
        send(&db, &latest, "Unrelated follow-up answer", true).await;

        store::ingest_telegram(&db, "42", 1, "42", "Look at my daily summaries", 10)
            .await
            .unwrap();
        let request = store::claim(&db).await.unwrap().unwrap();
        assert!(
            store::settings_requests::permitted(&db, &request)
                .await
                .unwrap()
        );
        let retained = read(&db, &request, recent(None, 5)).await.unwrap();
        assert_eq!(retained.routines.len(), 1);
        assert_eq!(retained.routines[0].id, "daily");
        let [today, yesterday] = retained.results.as_slice() else {
            panic!("expected two delivered initial replies");
        };
        assert_eq!(today.task, latest.id);
        assert_eq!(today.text, "Today's summary");
        assert_eq!(yesterday.task, old.id);
        assert_eq!(yesterday.text, "Yesterday's summary");
        assert!(
            read(&db, &request, recent(Some("private"), 5))
                .await
                .is_err()
        );
        assert_eq!(
            read(&db, &request, recent(Some("daily"), 1))
                .await
                .unwrap()
                .results
                .len(),
            1
        );

        add_routine(&db, "weekly", "42", "Weekly summary").await;
        sqlx::query("UPDATE tasks SET state='idle' WHERE id=?")
            .bind(&request.id)
            .execute(&db)
            .await
            .unwrap();
        run_and_reply(&db, "42", "weekly", 105, "Weekly digest", true).await;
        sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
            .bind(&request.id)
            .execute(&db)
            .await
            .unwrap();
        let choices = read(&db, &request, recent(None, 5)).await.unwrap();
        let [daily, weekly] = choices.routines.as_slice() else {
            panic!("expected owned routine choices");
        };
        assert_eq!(daily.id, "daily");
        assert_eq!(weekly.id, "weekly");
        assert!(choices.results.is_empty());

        add_routine(&db, "daily-other", "42", "Daily summary").await;
        let same_title = read(
            &db,
            &request,
            Query {
                title: Some("Daily Summary"),
                ..recent(None, 5)
            },
        )
        .await
        .unwrap();
        assert_eq!(same_title.routines.len(), 2);
        assert!(same_title.results.is_empty());

        sqlx::query("UPDATE tasks SET revision=revision+1 WHERE id=?")
            .bind(&request.id)
            .execute(&db)
            .await
            .unwrap();
        assert!(read(&db, &request, recent(Some("daily"), 5)).await.is_err());
    }
}
