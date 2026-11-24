use super::Task;
use crate::security::{id, now};
use anyhow::{Result, ensure};
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};

#[derive(FromRow, Serialize)]
pub struct Pair {
    id: String,
    peer: String,
    peer_name: Option<String>,
    joined: bool,
    peer_joined: bool,
    ready: bool,
}

pub async fn pairs(db: &SqlitePool, user: &str) -> Result<Vec<Pair>> {
    Ok(sqlx::query_as(
        "SELECT p.id,u.id AS peer,u.display_name AS peer_name,
                CASE WHEN p.owner=?1 THEN p.owner_opted_in ELSE p.guest_opted_in END AS joined,
                CASE WHEN p.owner=?1 THEN p.guest_opted_in ELSE p.owner_opted_in END AS peer_joined,
                live.id IS NOT NULL AS ready
         FROM shared_memory_pairs p JOIN users u ON u.id=CASE WHEN p.owner=?1 THEN p.guest ELSE p.owner END
         LEFT JOIN available_shared_memory_pairs live ON live.id=p.id
         WHERE p.owner=?1 OR p.guest=?1 ORDER BY coalesce(u.display_name,u.id) LIMIT 200",
    )
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn invite(db: &SqlitePool, operator: &str, guest: &str) -> Result<()> {
    ensure!(operator != guest, "choose a guest");
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let pair: Option<(String, bool)> = sqlx::query_as(
        "SELECT id,owner_opted_in FROM shared_memory_pairs WHERE owner=? AND guest=?",
    )
    .bind(operator)
    .bind(guest)
    .fetch_optional(&mut *tx)
    .await?;
    if pair.as_ref().is_some_and(|(_, opted_in)| *opted_in) {
        return Ok(());
    }
    let changed = sqlx::query(
        "INSERT INTO shared_memory_pairs(id,owner,guest,owner_opted_in)
         SELECT ?,?,id,1 FROM users WHERE id=? AND suspended_at IS NULL
         ON CONFLICT(owner,guest) DO UPDATE SET owner_opted_in=1",
    )
    .bind(id())
    .bind(operator)
    .bind(guest)
    .execute(&mut *tx)
    .await?;
    ensure!(changed.rows_affected() == 1, "guest unavailable");
    sqlx::query("INSERT INTO outbox(user,body,button) VALUES(?,?,'mini')")
        .bind(guest)
        .bind("The bot operator invited you to share selected memories. Open Memory in Settings & forms to review and choose whether to join.")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn set_joined(db: &SqlitePool, user: &str, pair: &str, joined: bool) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE shared_memory_pairs SET
            owner_opted_in=CASE WHEN owner=?1 THEN ?2 ELSE owner_opted_in END,
            guest_opted_in=CASE WHEN guest=?1 THEN ?2 ELSE guest_opted_in END
         WHERE id=?3 AND (owner=?1 OR guest=?1)
           AND (?2=0 OR CASE WHEN owner=?1 THEN guest_opted_in ELSE owner_opted_in END=1)",
    )
    .bind(user)
    .bind(joined)
    .bind(pair)
    .execute(db)
    .await?;
    ensure!(
        changed.rows_affected() == 1,
        "shared memory space unavailable"
    );
    Ok(())
}

#[derive(FromRow, Serialize)]
pub struct Note {
    id: String,
    body: String,
    own: bool,
    created_at: i64,
}

pub async fn notes(db: &SqlitePool, user: &str, pair: &str) -> Result<Vec<Note>> {
    let mut tx = db.begin().await?;
    let ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM available_shared_memory_pairs WHERE id=? AND (owner=? OR guest=?))",
    )
    .bind(pair)
    .bind(user)
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(ready, "shared memory space unavailable");
    let notes = sqlx::query_as(
        "SELECT id,body,created_at,author=? AS own FROM shared_memory_notes WHERE pair=?
         ORDER BY created_at DESC,rowid DESC LIMIT 100",
    )
    .bind(user)
    .bind(pair)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(notes)
}

pub async fn publish(db: &SqlitePool, user: &str, pair: &str, body: &str) -> Result<()> {
    let body = body.trim();
    ensure!(
        !body.is_empty() && body.len() <= 1200 && !body.contains('\0'),
        "invalid memory"
    );
    let changed = sqlx::query(
        "INSERT INTO shared_memory_notes(id,pair,author,body,created_at)
         SELECT ?,id,?,?,? FROM available_shared_memory_pairs
         WHERE id=? AND (owner=? OR guest=?)
           AND (SELECT count(*) FROM shared_memory_notes WHERE pair=?)<100",
    )
    .bind(id())
    .bind(user)
    .bind(body)
    .bind(now())
    .bind(pair)
    .bind(user)
    .bind(user)
    .bind(pair)
    .execute(db)
    .await?;
    ensure!(
        changed.rows_affected() == 1,
        "shared memory space unavailable or full"
    );
    Ok(())
}

pub async fn remove(db: &SqlitePool, user: &str, pair: &str, note: &str) -> Result<()> {
    let changed = sqlx::query("DELETE FROM shared_memory_notes WHERE id=? AND pair=? AND author=?")
        .bind(note)
        .bind(pair)
        .bind(user)
        .execute(db)
        .await?;
    ensure!(
        changed.rows_affected() == 1,
        "shared memory note unavailable"
    );
    Ok(())
}

pub async fn open(db: &SqlitePool, user: &str, pair: &str) -> Result<()> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let task = id();
    let created = sqlx::query(
        "INSERT INTO tasks(id,user,title,state,updated,shared_memory_pair)
         SELECT ?,?,'Shared memory','idle',?,id FROM available_shared_memory_pairs
         WHERE id=? AND (owner=? OR guest=?)",
    )
    .bind(&task)
    .bind(user)
    .bind(now())
    .bind(pair)
    .bind(user)
    .bind(user)
    .execute(&mut *tx)
    .await?;
    ensure!(
        created.rows_affected() == 1,
        "shared memory space unavailable"
    );
    sqlx::query("UPDATE users SET active_task=? WHERE id=?")
        .bind(&task)
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO outbox(user,task,body) VALUES(?,?,?)")
        .bind(user)
        .bind(task)
        .bind("Shared memory chat is ready. Send your next message here. Use /new to return to a private task.")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn context(db: &SqlitePool, task: &Task) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT n.body FROM shared_memory_notes n JOIN available_shared_memory_pairs p ON p.id=n.pair
         JOIN tasks t ON t.shared_memory_pair=p.id
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running' AND (p.owner=t.user OR p.guest=t.user)
         ORDER BY n.created_at DESC,n.rowid DESC LIMIT 16",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_all(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    #[tokio::test]
    async fn pair_consent_scopes_notes_and_revocation_invalidates_queued_results() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        for user in ["1", "2", "3"] {
            store::seed(&db, user).await.unwrap();
        }
        sqlx::query("UPDATE users SET provider='codex'")
            .execute(&db)
            .await
            .unwrap();
        let (owner, guest, outsider) = ("1", "2", "3");
        invite(&db, owner, guest).await.unwrap();
        let pair = pairs(&db, guest).await.unwrap().remove(0).id;
        assert!(!pairs(&db, guest).await.unwrap()[0].ready);
        assert!(notes(&db, owner, &pair).await.is_err());
        assert!(set_joined(&db, outsider, &pair, true).await.is_err());
        set_joined(&db, guest, &pair, true).await.unwrap();
        publish(&db, owner, &pair, "Owner’s shared note")
            .await
            .unwrap();
        publish(&db, guest, &pair, "Guest’s shared note")
            .await
            .unwrap();
        let notes_for_guest = notes(&db, guest, &pair).await.unwrap();
        let owner_note = notes_for_guest.iter().find(|note| !note.own).unwrap();
        assert!(remove(&db, guest, &pair, &owner_note.id).await.is_err());
        assert!(notes(&db, outsider, &pair).await.is_err());
        assert!(open(&db, outsider, &pair).await.is_err());

        store::ingest(&db, owner, "test", 1, guest, "Private question")
            .await
            .unwrap();
        let private_task = store::claim(&db).await.unwrap().unwrap();
        assert!(context(&db, &private_task).await.unwrap().is_empty());
        store::settle(&db, &private_task, "idle", None)
            .await
            .unwrap();
        open(&db, guest, &pair).await.unwrap();
        store::ingest(&db, owner, "test", 2, guest, "What are our notes?")
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(task.shared_memory_pair.as_deref(), Some(pair.as_str()));
        assert_eq!(context(&db, &task).await.unwrap().len(), 2);
        assert!(!store::select_task(&db, owner, &task.id).await.unwrap());
        store::settle(&db, &task, "idle", Some("Shared result"))
            .await
            .unwrap();
        remove(&db, owner, &pair, &owner_note.id).await.unwrap();
        let mut revoked_result = false;
        while let Some(item) = store::claim_outbox(&db).await.unwrap() {
            if item.body == "Shared result" {
                revoked_result = true;
                assert!(!store::can_deliver(&db, &item).await.unwrap());
            }
        }
        assert!(revoked_result);
        assert!(
            store::select_task(&db, guest, &private_task.id)
                .await
                .unwrap()
        );
        set_joined(&db, guest, &pair, false).await.unwrap();
        assert!(notes(&db, owner, &pair).await.is_err());
        assert!(open(&db, guest, &pair).await.is_err());
    }
}
