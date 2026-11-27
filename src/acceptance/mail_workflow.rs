use super::{app_with_model, fixture, message, pending_id, signed_mini_init};
use crate::{
    App,
    model::{ModelProvider, TextDelta, ToolDefinition},
    runtime,
    store::{self, McpApprovalDecision, Task, mail},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

const BODY: &str = "The draft contains a private reference for the customer.";
const RECIPIENT: &str = "recipient@example.net";

struct MailChief;

#[async_trait]
impl ModelProvider for MailChief {
    async fn complete(&self, system: &str, _: &str) -> Result<String> {
        if system.starts_with("Return JSON with authorized") {
            return Ok(json!({"authorized":true}).to_string());
        }
        Ok(json!({"text":"Nothing was sent."}).to_string())
    }

    async fn complete_turn(
        &self,
        _: &str,
        input: &str,
        tools: &[ToolDefinition],
        _: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        let context: Value = serde_json::from_str(input)?;
        assert!(tools.iter().any(|tool| tool.name == "send_email"));
        let response = if input.contains("already handled or declined")
            || input.contains("Nothing was sent")
        {
            json!({"kind":"reply","text":"Nothing was sent."})
        } else {
            // Repeat the draft after denial to verify the runtime still blocks it.
            json!({"kind":"tool","name":"send_email","args":{
                "account":context["mail_accounts"][0]["id"],
                "to":[RECIPIENT],"subject":"Customer update","body":BODY,"execute":context["latest_request"].as_str().is_some_and(|s|s.contains("directly"))
            }})
        };
        Ok(response.to_string())
    }
}

fn account_config(label: &str) -> mail::MailConfig {
    mail::MailConfig {
        label: label.into(),
        address: "sender@example.com".into(),
        imap: None,
        smtp: Some(mail::MailCredentials {
            host: "smtp.example.com".into(),
            port: 465,
            username: "sender@example.com".into(),
            password: "fixture-password".into(),
        }),
        folders: Vec::new(),
    }
}

async fn prepare() -> (App, Task, String, String) {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(MailChief));
    let account = mail::save_account(&db, &app.vault, "1", None, account_config("Test mail"))
        .await
        .unwrap();
    message(&db, 42, "1", "Email the customer with an update").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    let request = pending_id(&db, &task, "email-send").await;
    (app, task, account, request)
}

#[tokio::test]
async fn smtp_draft_is_private_and_deny_cannot_be_replayed_or_bypassed() {
    let (app, task, _, request) = prepare().await;
    let pending = store::list_requests(&app.db, "1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending[0].payload.contains(BODY));
    assert!(!pending[0].payload.contains(RECIPIENT));
    let payload = serde_json::from_str(&pending[0].payload).unwrap();
    let preview = runtime::smtp_preview(&app.vault, "1", &request, &payload).unwrap();
    for field in ["sender@example.com", RECIPIENT, "Customer update", BODY] {
        assert!(preview.contains(field));
    }
    assert!(runtime::smtp_preview(&app.vault, "2", &request, &payload).is_err());
    assert!(
        store::respond_email(&app.db, "2", &request, true)
            .await
            .is_err()
    );
    assert!(
        store::respond_mcp(&app.db, "1", &request, McpApprovalDecision::Always)
            .await
            .is_err()
    );
    store::respond_email(&app.db, "1", &request, false)
        .await
        .unwrap();
    assert!(
        store::respond_email(&app.db, "1", &request, true)
            .await
            .is_err()
    );
    let resumed = store::claim(&app.db).await.unwrap().unwrap();
    assert_eq!(resumed.id, task.id);
    runtime::work(&app, &resumed).await.unwrap();
    assert!(store::list_requests(&app.db, "1").await.unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pending WHERE kind='email-send'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn smtp_approval_stops_when_account_changes_before_execution() {
    let (app, _, account, request) = prepare().await;
    store::respond(&app.db, "1", &request, true).await.unwrap();
    assert!(
        store::respond_email(&app.db, "1", &request, true)
            .await
            .is_err()
    );
    mail::save_account(
        &app.db,
        &app.vault,
        "1",
        Some(&account),
        account_config("Edited mail"),
    )
    .await
    .unwrap();
    let resumed = store::claim(&app.db).await.unwrap().unwrap();
    runtime::work(&app, &resumed).await.unwrap();
    assert!(
        store::history(&app.db, &resumed.id)
            .await
            .unwrap()
            .iter()
            .any(|(_, role, text)| role == "tool" && text.contains("Nothing was sent"))
    );
}

#[tokio::test]
async fn interrupted_smtp_approval_cannot_send_again_after_restart() {
    let (app, task, _, request) = prepare().await;
    store::respond_email(&app.db, "1", &request, true)
        .await
        .unwrap();
    let resumed = store::claim(&app.db).await.unwrap().unwrap();
    store::claim_approval(&app.db, &resumed)
        .await
        .unwrap()
        .unwrap();
    store::recover(&app.db).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM tasks WHERE id=?")
        .bind(task.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(state, "uncertain");
    assert!(store::claim(&app.db).await.unwrap().is_none());
}

#[tokio::test]
async fn editing_a_draft_requires_its_owner_and_a_fresh_approval_for_exact_mail() {
    let (app, task, _, old) = prepare().await;
    let app = Arc::new(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/mini", listener.local_addr().unwrap());
    let router = crate::web::router(app.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let draft = json!({"to":["updated@example.net"],"subject":"An edited subject","body":"New draft text.\r\nSecond line."});
    let edit = |user| {
        client.post(&endpoint).header("origin", &app.origin).json(&json!({
        "init":signed_mini_init(&app, user),"action":"edit-email-draft","data":{"id":old,"draft":draft}
    })).send()
    };
    assert_eq!(edit(2).await.unwrap().status(), 400);
    let result: Value = edit(1)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let next = result["id"].as_str().unwrap();
    assert_ne!(next, old);
    assert!(
        store::respond_email(&app.db, "1", &old, true)
            .await
            .is_err()
    );
    assert!(
        store::respond_email(&app.db, "2", next, true)
            .await
            .is_err()
    );
    assert!(store::claim(&app.db).await.unwrap().is_none());
    store::respond_email(&app.db, "1", next, true)
        .await
        .unwrap();
    let resumed = store::claim(&app.db).await.unwrap().unwrap();
    assert_eq!(resumed.id, task.id);
    let approved = store::claim_approval(&app.db, &resumed)
        .await
        .unwrap()
        .unwrap();
    let formatted =
        String::from_utf8(runtime::smtp_approved_message(&app.vault, "1", &approved).unwrap())
            .unwrap();
    assert!(formatted.contains("From: sender@example.com\r\n"));
    assert!(formatted.contains("To: updated@example.net\r\n"));
    assert!(formatted.contains("Subject: An edited subject\r\n"));
    assert!(formatted.contains("\r\n\r\nNew draft text.\r\nSecond line."));
    assert!(!formatted.contains(BODY) && !formatted.contains(RECIPIENT));
    server.abort();
}

#[tokio::test]
async fn saving_mail_for_a_task_resumes_only_the_signed_owners_waiting_revision() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(MailChief));
    message(&db, 91, "1", "Connect my work sender and send the update").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    let id = crate::security::id();
    store::pause(
        &db,
        &task,
        "mail-setup",
        &json!({"capability":"smtp"}),
        "Connect mail",
        Some(&format!("mail-setup/{id}")),
        900,
        &id,
    )
    .await
    .unwrap();
    assert!(
        mail::save_for_task(
            &db,
            &app.vault,
            "2",
            None,
            account_config("Work"),
            Some(&id)
        )
        .await
        .is_err()
    );
    assert!(mail::smtp_accounts(&db, "2").await.unwrap().is_empty());
    let account = mail::save_for_task(
        &db,
        &app.vault,
        "1",
        None,
        account_config("Work"),
        Some(&id),
    )
    .await
    .unwrap();
    assert!(
        mail::save_for_task(
            &db,
            &app.vault,
            "1",
            None,
            account_config("Work"),
            Some(&id)
        )
        .await
        .is_err()
    );
    assert_eq!(mail::smtp_accounts(&db, "1").await.unwrap().len(), 1);
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.id, task.id);
    assert!(
        store::history(&db, &task.id)
            .await
            .unwrap()
            .last()
            .unwrap()
            .2
            .contains(&account)
    );
}

#[tokio::test]
async fn sending_directly_stops_if_the_owner_turns_review_back_on() {
    let db = fixture().await;
    let app = app_with_model(&db, Arc::new(MailChief));
    let account = mail::save_account(&db, &app.vault, "1", None, account_config("Work"))
        .await
        .unwrap();
    store::ingest_telegram(
        &db,
        "1",
        51,
        "1",
        "Send directly to recipient@example.net",
        51,
    )
    .await
    .unwrap();
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert!(store::list_requests(&db, "1").await.unwrap().is_empty());
    sqlx::query("UPDATE mail_accounts SET automatic_actions=0 WHERE id=?")
        .bind(account)
        .execute(&db)
        .await
        .unwrap();
    let resumed = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &resumed).await.unwrap();
    assert!(store::history(&db,&task.id).await.unwrap().iter().any(|(_,role,body)| role=="tool" && body.contains("policy or instruction changed")));
}
