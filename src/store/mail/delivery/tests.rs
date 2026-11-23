use super::*;
use crate::store::{
    self, ReplyContext,
    mail::{
        FolderConfig, MailConfig, list_accounts, remove_account, save_account, smtp_connection,
    },
};
use sqlx::sqlite::SqlitePoolOptions;

async fn fixture() -> (SqlitePool, Vault) {
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
    (db, Vault::new(&"a0".repeat(32)).unwrap())
}

fn config(automate: bool) -> MailConfig {
    MailConfig {
        label: "Work".into(),
        address: "owner@example.test".into(),
        imap: Some(MailCredentials {
            host: "imap.example.test".into(),
            username: "owner".into(),
            password: "test-imap-password".into(),
            port: 993,
        }),
        smtp: Some(MailCredentials {
            host: "smtp.example.test".into(),
            username: "owner".into(),
            password: "test-smtp-password".into(),
            port: 587,
        }),
        folders: vec![FolderConfig {
            name: "INBOX".into(),
            automate,
            instruction: automate.then(|| "Summarize the message for me.".into()),
        }],
    }
}

fn content() -> MailContent {
    MailContent {
        from: "private-sender@example.test".into(),
        reply_to: Some("private-reply@example.test".into()),
        subject: "A confidential subject".into(),
        text: "This body is confidential.".into(),
        message_id: Some("<private-thread@example.test>".into()),
    }
}

async fn baseline(db: &SqlitePool, vault: &Vault) -> PollMailbox {
    let mut mailbox = poll_mailboxes(db, vault).await.unwrap().remove(0);
    assert!(initialize_cursor(db, &mailbox, 10, 100).await.unwrap());
    assert!(!initialize_cursor(db, &mailbox, 10, 100).await.unwrap());
    mailbox.uid_validity = Some(10);
    mailbox.last_uid = 100;
    mailbox
}

async fn sent_notice(db: &SqlitePool, message_id: i64) -> (String, String) {
    let notice = store::claim_outbox(db).await.unwrap().unwrap();
    assert!(notice.body.starts_with("New email in Work (INBOX)"));
    assert!(!notice.body.contains("confidential") && !notice.body.contains("private-sender"));
    sqlx::query("UPDATE outbox SET state='sent',telegram_message_id=? WHERE id=?")
        .bind(message_id)
        .bind(notice.id)
        .execute(db)
        .await
        .unwrap();
    (
        notice
            .button
            .unwrap()
            .strip_prefix("mail-read/")
            .unwrap()
            .into(),
        notice.task.unwrap(),
    )
}

#[tokio::test]
async fn owner_can_read_one_message_then_act_on_it_without_exposing_other_mail() {
    let (db, vault) = fixture().await;
    let account = save_account(&db, &vault, "1", None, config(false))
        .await
        .unwrap();
    let mut mailbox = baseline(&db, &vault).await;
    assert!(
        record_delivery(&db, &vault, &mailbox, 10, 101, &content())
            .await
            .unwrap()
    );
    assert!(
        !record_delivery(&db, &vault, &mailbox, 10, 101, &content())
            .await
            .unwrap()
    );
    mailbox.last_uid = 101;
    assert!(
        record_delivery(&db, &vault, &mailbox, 10, 102, &content())
            .await
            .unwrap()
    );
    let (message, mail_task) = sent_notice(&db, 201).await;
    let (other_message, other_task) = sent_notice(&db, 202).await;
    assert!(store::claim(&db).await.unwrap().is_none());
    assert!(store::history(&db, &mail_task).await.unwrap().is_empty());

    let ciphertext: String = sqlx::query_scalar("SELECT encrypted FROM mail_messages WHERE id=?")
        .bind(&message)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!ciphertext.contains("confidential"));
    assert!(open_message(&vault, "2", &message, &ciphertext).is_err());
    assert!(open_message(&vault, "1", &other_message, &ciphertext).is_err());

    assert!(!reveal_mail(&db, "2", &message, 201).await.unwrap());
    assert!(!reveal_mail(&db, "1", &message, 202).await.unwrap());
    assert!(!reveal_mail(&db, "1", &other_message, 201).await.unwrap());
    store::ingest_telegram_reaction(&db, "1", 1, "1", 201, "👍")
        .await
        .unwrap();
    assert!(store::claim(&db).await.unwrap().is_none());
    store::ingest_telegram(&db, "1", 2, "1", "Something else", 203)
        .await
        .unwrap();
    let unrelated = store::claim(&db).await.unwrap().unwrap();
    assert_ne!(unrelated.id, mail_task);
    assert!(
        task_context(&db, &vault, &unrelated)
            .await
            .unwrap()
            .is_none()
    );
    store::settle(&db, &unrelated, "idle", None).await.unwrap();

    assert!(reveal_mail(&db, "1", &message, 201).await.unwrap());
    assert!(!reveal_mail(&db, "1", &message, 201).await.unwrap());
    assert!(store::claim(&db).await.unwrap().is_none());
    let preview = store::claim_outbox(&db).await.unwrap().unwrap();
    assert!(preview.body.is_empty());
    assert_eq!(preview.reply_to_message_id, Some(201));
    assert!(
        mail_preview(&db, &vault, &preview, &message)
            .await
            .unwrap()
            .unwrap()
            .contains("A confidential subject")
    );
    assert!(
        mail_preview(&db, &vault, &preview, &other_message)
            .await
            .unwrap()
            .is_none()
    );
    store::ingest_telegram(&db, "1", 3, "1", "Summarize it", 204)
        .await
        .unwrap();
    let opened = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(opened.id, mail_task);
    let context = task_context(&db, &vault, &opened).await.unwrap().unwrap();
    assert!(matches!(context.access, MailAccess::Revealed));
    assert_eq!(context.content.unwrap().subject, content().subject);
    assert!(!context.automated_turn);
    assert!(remove_account(&db, "1", &account).await.unwrap());
    let active: Option<String> = sqlx::query_scalar("SELECT active_task FROM users WHERE id='1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(active.is_none());
    assert!(task_context(&db, &vault, &opened).await.unwrap().is_none());
    assert!(
        mail_preview(&db, &vault, &preview, &message)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!reveal_mail(&db, "1", &other_message, 202).await.unwrap());
    assert!(store::history(&db, &other_task).await.unwrap().is_empty());
}

#[tokio::test]
async fn exact_reply_opens_the_email_for_that_request_while_switching_alone_keeps_it_redacted() {
    let (db, vault) = fixture().await;
    save_account(&db, &vault, "1", None, config(false))
        .await
        .unwrap();
    let mailbox = baseline(&db, &vault).await;
    record_delivery(&db, &vault, &mailbox, 10, 101, &content())
        .await
        .unwrap();
    let (_, mail_task) = sent_notice(&db, 201).await;
    assert!(store::select_task(&db, "1", &mail_task).await.unwrap());
    store::ingest_telegram(&db, "1", 1, "1", "What arrived?", 202)
        .await
        .unwrap();
    let hidden = store::claim(&db).await.unwrap().unwrap();
    let context = task_context(&db, &vault, &hidden).await.unwrap().unwrap();
    assert!(matches!(context.access, MailAccess::Redacted));
    assert!(context.content.is_none());
    store::settle(&db, &hidden, "idle", None).await.unwrap();

    store::ingest_telegram_reply(
        &db,
        "1",
        2,
        "1",
        "What do they need?",
        203,
        Some(ReplyContext {
            message_id: 201,
            from_bot: true,
            excerpt: None,
        }),
    )
    .await
    .unwrap();
    let opened = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(opened.id, mail_task);
    assert_eq!(
        store::history(&db, &opened.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2,
        "What do they need?"
    );
    let context = task_context(&db, &vault, &opened).await.unwrap().unwrap();
    assert!(matches!(context.access, MailAccess::Revealed));
    assert_eq!(context.content.unwrap().text, content().text);
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
}

#[tokio::test]
async fn automation_keeps_email_out_of_events_and_stops_at_the_configuration_epoch() {
    let (db, vault) = fixture().await;
    let account = save_account(&db, &vault, "1", None, config(true))
        .await
        .unwrap();
    let mut mailbox = baseline(&db, &vault).await;
    assert!(advance_cursor(&db, &mailbox, 10, 105).await.unwrap());
    assert!(!advance_cursor(&db, &mailbox, 10, 107).await.unwrap());
    mailbox.last_uid = 105;
    assert!(
        record_delivery(&db, &vault, &mailbox, 10, 106, &content())
            .await
            .unwrap()
    );
    let automatic = store::claim(&db).await.unwrap().unwrap();
    assert!(store::claim_outbox(&db).await.unwrap().is_none());
    let history = store::history(&db, &automatic.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].2, "Summarize the message for me.");
    let context = task_context(&db, &vault, &automatic)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(context.access, MailAccess::Automatic));
    assert_eq!(context.content.unwrap().message_id, content().message_id);
    assert!(context.automated_turn);

    let mut edit = config(false);
    edit.imap.as_mut().unwrap().password.clear();
    edit.smtp.as_mut().unwrap().password.clear();
    assert!(
        save_account(&db, &vault, "2", Some(&account), config(false))
            .await
            .is_err()
    );
    save_account(&db, &vault, "1", Some(&account), edit)
        .await
        .unwrap();
    let context = task_context(&db, &vault, &automatic)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(context.access, MailAccess::Redacted));
    assert!(context.content.is_none());
    assert!(context.automated_turn);
    mailbox.last_uid = 106;
    assert!(
        !record_delivery(&db, &vault, &mailbox, 10, 107, &content())
            .await
            .unwrap()
    );
    let fresh = poll_mailboxes(&db, &vault).await.unwrap().remove(0);
    assert_eq!(fresh.last_uid, 106);
    assert_eq!(fresh.imap.password, "test-imap-password");
    assert_eq!(
        smtp_connection(&db, &vault, "1", &account)
            .await
            .unwrap()
            .credentials
            .password,
        "test-smtp-password"
    );
    assert!(smtp_connection(&db, &vault, "2", &account).await.is_err());
    let summary = serde_json::to_string(&list_accounts(&db, &vault, "1").await.unwrap()).unwrap();
    assert!(!summary.contains("password"));
}
