use super::{app_with_model, fixture, message, pending_id};
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
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
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
                "to":[RECIPIENT],"subject":"Customer update","body":BODY
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
