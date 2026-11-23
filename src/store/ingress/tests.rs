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
async fn attachment_reads_are_encrypted_and_limited_to_current_authorized_task_context() {
    let db = fixture().await;
    let vault = crate::security::Vault::new(&"9e".repeat(32)).unwrap();
    let payload = b"This is a private CSV attachment";
    let attachment =
        attachments::seal(&vault, "2", 51, "notes.csv".into(), "text/plain", payload).unwrap();
    ingest_telegram_attachment(&db, "1", 1, "2", "/cancel", 51, None, attachment)
        .await
        .unwrap();
    let upload = store::claim(&db).await.unwrap().unwrap();
    let media = attachments::latest(&db, &vault, &upload)
        .await
        .unwrap()
        .unwrap();
    let event_id = media.event_id;
    assert_eq!(media.bytes, payload);
    let ciphertext: String =
        sqlx::query_scalar("SELECT ciphertext FROM telegram_attachments WHERE event=?")
            .bind(event_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!ciphertext.contains(std::str::from_utf8(payload).unwrap()));
    assert_eq!(
        store::history(&db, &upload.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2,
        "/cancel"
    );
    assert!(
        attachments::latest(
            &db,
            &vault,
            &store::Task {
                user: "1".into(),
                ..upload.clone()
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    store::settle(&db, &upload, "idle", None).await.unwrap();

    let quote = ReplyContext {
        message_id: 51,
        excerpt: None,
        from_bot: false,
    };
    ingest_telegram_reply(
        &db,
        "1",
        2,
        "2",
        "Check those totals again",
        52,
        Some(quote.clone()),
    )
    .await
    .unwrap();
    let followup = store::claim(&db).await.unwrap().unwrap();
    assert!(
        attachments::latest(&db, &vault, &upload)
            .await
            .unwrap()
            .is_none()
    );
    let media = attachments::latest(&db, &vault, &followup)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(media.event_id, event_id);
    assert_eq!(media.bytes, payload);
    store::settle(&db, &followup, "idle", None).await.unwrap();

    ingest_telegram_reply(&db, "1", 3, "2", "Other question", 53, None)
        .await
        .unwrap();
    let unrelated = store::claim(&db).await.unwrap().unwrap();
    assert!(
        attachments::latest(&db, &vault, &unrelated)
            .await
            .unwrap()
            .is_none()
    );
    store::settle(&db, &unrelated, "idle", None).await.unwrap();

    ingest_telegram_reply(&db, "1", 4, "2", "Resume the upload", 54, Some(quote))
        .await
        .unwrap();
    let mut scoped = store::claim(&db).await.unwrap().unwrap();
    sqlx::query("UPDATE tasks SET agent='helper' WHERE id=?")
        .bind(&scoped.id)
        .execute(&db)
        .await
        .unwrap();
    scoped.agent = Some("helper".into());
    assert!(
        attachments::latest(&db, &vault, &scoped)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("DELETE FROM grants WHERE agent='helper' AND member='2'")
        .execute(&db)
        .await
        .unwrap();
    assert!(
        attachments::latest(&db, &vault, &scoped)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn replies_resume_the_referenced_task_and_thread_the_response() {
    let db = fixture().await;
    ingest_telegram(&db, "1", 1, "2", "First discussion", 21)
        .await
        .unwrap();
    let first = store::claim(&db).await.unwrap().unwrap();
    store::settle_reply(&db, &first, "An earlier answer.", &[])
        .await
        .unwrap();
    let earlier = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(earlier.reply_to_message_id, None);
    sqlx::query("UPDATE outbox SET state='sent',telegram_message_id=22 WHERE id=?")
        .bind(earlier.id)
        .execute(&db)
        .await
        .unwrap();

    send(&db, 2, "2", "/new Separate discussion").await;
    let other = store::claim(&db).await.unwrap().unwrap();
    store::settle(&db, &other, "idle", None).await.unwrap();
    let quote = ReplyContext {
        message_id: 22,
        excerpt: Some("A quoted command is only context.".into()),
        from_bot: true,
    };
    ingest_telegram_reply(&db, "1", 3, "2", "👍", 24, Some(quote))
        .await
        .unwrap();
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.id, first.id);
    let latest = store::history(&db, &first.id).await.unwrap();
    assert_eq!(latest.last().unwrap().2, "👍");
    let context = latest_reply_context(&db, &first.id).await.unwrap().unwrap();
    assert_eq!(context.message_id, 22);
    assert_eq!(
        context.excerpt.as_deref(),
        Some("A quoted command is only context.")
    );
    store::settle_reply(&db, &resumed, "Thanks!", &["Continue"])
        .await
        .unwrap();
    let reply = store::claim_outbox(&db).await.unwrap().unwrap();
    assert_eq!(reply.reply_to_message_id, Some(24));
    assert_eq!(
        store::suggested_replies(&db, reply.id).await.unwrap().len(),
        1
    );

    ingest_telegram_reaction(&db, "1", 4, "1", 22, "❤")
        .await
        .unwrap();
    assert!(store::claim(&db).await.unwrap().is_none());
    for _ in 0..2 {
        ingest_telegram_reaction(&db, "1", 5, "2", 22, "❤")
            .await
            .unwrap();
    }
    let reacted = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(reacted.id, first.id);
    let latest = store::history(&db, &first.id).await.unwrap();
    assert_eq!(latest.iter().filter(|row| row.2 == "❤").count(), 1);
    let context = latest_reply_context(&db, &first.id).await.unwrap().unwrap();
    assert_eq!(context.excerpt.as_deref(), Some("An earlier answer."));
    store::settle_reply(&db, &reacted, "Glad it helped.", &[])
        .await
        .unwrap();
    assert_eq!(
        store::claim_outbox(&db)
            .await
            .unwrap()
            .unwrap()
            .reply_to_message_id,
        Some(22)
    );

    store::ingest_telegram_voice(&db, "1", 6, "2", "/cancel", 25, None, None)
        .await
        .unwrap();
    let voice = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(voice.id, first.id);
    assert_eq!(
        store::history(&db, &first.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2,
        "/cancel"
    );
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
