use super::*;
use crate::store;
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
        sqlx::query("UPDATE users SET provider='codex' WHERE id=?")
            .bind(user)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO agents VALUES('helper','1','Research','Find sources.','[]')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO grants VALUES('helper','2')")
        .execute(&db)
        .await
        .unwrap();
    db
}

async fn send(db: &SqlitePool, update: i64, user: &str, text: &str) {
    ingest(db, "1", "test", update, user, text).await.unwrap();
}

#[tokio::test]
async fn legacy_child_chat_and_buttons_are_archived_and_next_message_goes_to_the_chief() {
    let db = fixture().await;
    sqlx::query(
        "INSERT INTO tasks(id,user,agent,chief_chat,title,state,updated)
         VALUES('old-chat','2','helper',0,'Old shared conversation','idle',1)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO events(task,role,body) VALUES('old-chat','user','Old question')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET active_task='old-chat' WHERE id='2'")
        .execute(&db)
        .await
        .unwrap();
    let outbox = sqlx::query(
        "INSERT INTO outbox(user,task,agent,body,state) VALUES('2','old-chat','helper','','sent')",
    )
    .execute(&db)
    .await
    .unwrap()
    .last_insert_rowid();
    sqlx::query("INSERT INTO suggested_replies VALUES('old-reply',?,0,'Old follow-up',?)")
        .bind(outbox)
        .bind(now() + 600)
        .execute(&db)
        .await
        .unwrap();

    send(&db, 1, "2", "/use helper").await;
    send(&db, 2, "2", "/switch old-chat").await;
    send(&db, 3, "2", "/tasks").await;
    for text in [
        "Message your Chief",
        "Task unavailable.",
        "You have no tasks yet.",
    ] {
        let reply = store::claim_outbox(&db).await.unwrap().unwrap();
        assert!(reply.body.starts_with(text));
    }
    assert!(!store::select_task(&db, "2", "old-chat").await.unwrap());
    assert!(!store::redeem_reply(&db, "2", "old-reply").await.unwrap());
    assert!(store::list_tasks(&db, "2").await.unwrap().is_empty());

    send(&db, 4, "2", "New question for my Chief").await;
    let (id, agent, chief_chat, state): (String, Option<String>, bool, String) = sqlx::query_as(
        "SELECT t.id,t.agent,t.chief_chat,t.state FROM users u JOIN tasks t ON t.id=u.active_task WHERE u.id='2'",
    )
    .fetch_one(&db).await.unwrap();
    assert_ne!(id, "old-chat");
    assert_eq!(agent, None);
    assert!(chief_chat);
    assert_eq!(state, "queued");
    assert_eq!(
        store::history(&db, &id).await.unwrap()[0].2,
        "New question for my Chief"
    );
    assert_eq!(
        store::history(&db, "old-chat").await.unwrap()[0].2,
        "Old question"
    );
    let audit_message: String =
        sqlx::query_scalar("SELECT message FROM audit_sessions WHERE task=?")
            .bind(&id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(audit_message, "New question for my Chief");
}

#[tokio::test]
async fn chief_chats_keep_shared_permissions_and_invites_grant_without_selecting() {
    let db = fixture().await;
    sqlx::query(
        "INSERT INTO tasks(id,user,agent,chief_chat,title,state,updated)
         VALUES('scoped-chat','2','helper',1,'Chief with Research','idle',1)",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        store::list_tasks(&db, "2").await.unwrap()[0].id,
        "scoped-chat"
    );
    assert!(!store::select_task(&db, "1", "scoped-chat").await.unwrap());
    send(&db, 1, "2", "/switch scoped-chat").await;
    send(&db, 2, "2", "Ask Research to check this.").await;
    assert_eq!(
        store::history(&db, "scoped-chat").await.unwrap()[0].2,
        "Ask Research to check this."
    );
    sqlx::query("DELETE FROM grants WHERE agent='helper' AND member='2'")
        .execute(&db)
        .await
        .unwrap();
    send(&db, 3, "2", "Please continue.").await;
    assert_eq!(store::history(&db, "scoped-chat").await.unwrap().len(), 1);
    assert!(!store::select_task(&db, "2", "scoped-chat").await.unwrap());
    assert!(store::list_tasks(&db, "2").await.unwrap().is_empty());

    sqlx::query("INSERT INTO invitations VALUES(?,'1','helper',?)")
        .bind(hash("invitation"))
        .bind(now() + 600)
        .execute(&db)
        .await
        .unwrap();
    send(&db, 4, "3", "/start invitation").await;
    let active: Option<String> = sqlx::query_scalar("SELECT active_task FROM users WHERE id='3'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(active, None);
    assert!(store::has_access(&db, "3", Some("helper")).await.unwrap());
    let body: String = sqlx::query_scalar("SELECT body FROM outbox WHERE user='3'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(body.contains("Ask your Chief to consult Research"));
}
