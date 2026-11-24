use crate::{
    model::{ProviderFailure, provider_failure},
    store::Task,
};
use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

pub(super) struct Failure {
    pub category: &'static str,
    pub message: &'static str,
    pub retryable: bool,
}

pub(super) fn classify(error: &anyhow::Error) -> Failure {
    let (category, message, retryable) = match provider_failure(error) {
        Some(ProviderFailure::Setup) => (
            "model_setup_required",
            "This task needs a model. Open Settings & forms → Model to choose one.",
            false,
        ),
        Some(ProviderFailure::OpenRouterKey) => (
            "openrouter_key_required",
            "OpenRouter needs a working key. Open Settings & forms → Model to check or replace it.",
            false,
        ),
        Some(ProviderFailure::OpenRouterCredits) => (
            "openrouter_credit_limit",
            "OpenRouter reported a credit or spending limit. Check your OpenRouter balance and key limits.",
            false,
        ),
        Some(ProviderFailure::CodexSignIn) => (
            "codex_sign_in_required",
            "The ChatGPT connection needs to be reauthorized. Open Settings & forms → Model to reconnect it.",
            false,
        ),
        Some(ProviderFailure::CodexService) => (
            "codex_service_unavailable",
            "The Codex service on this TinyBot server is unavailable. Choose another connected model or ask the bot operator to check it.",
            false,
        ),
        Some(ProviderFailure::BackupDisabled) => (
            "backup_model_disabled",
            "The backup model is no longer enabled. Check your task and backup models in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::ModelUnavailable) => (
            "model_not_available",
            "The selected model is unavailable for this connection. Choose another model in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::RateLimited) => (
            "model_rate_limited",
            "The model provider reached a rate or usage limit. Give it some time before continuing.",
            true,
        ),
        Some(ProviderFailure::TimedOut) => (
            "model_timed_out",
            "The model provider took too long to respond. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Temporary) => (
            "model_temporarily_unavailable",
            "The model provider is temporarily unavailable. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Incomplete) => (
            "model_reply_incomplete",
            "The model didn’t return a usable reply. If it keeps happening, choose another task model in Settings & forms.",
            true,
        ),
        Some(ProviderFailure::InputLimit) => (
            "model_context_limit",
            "The model could not fit the task context into its context window.",
            false,
        ),
        Some(ProviderFailure::OutputLimit) => (
            "model_output_limit",
            "The model reached its response limit before it finished the answer.",
            false,
        ),
        Some(ProviderFailure::Refused) => (
            "model_refused",
            "The model declined this request. Send a different request to continue.",
            false,
        ),
        Some(ProviderFailure::AccessDenied) => (
            "model_access_denied",
            "The model provider blocked this request. Check the permissions for your connected account in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::Rejected) => (
            "model_request_rejected",
            "The model provider rejected this request. Check that your selected model supports tools, or choose another model in Settings & forms.",
            false,
        ),
        None => (
            "task_failed",
            "I paused this task after an internal error. Open Audit in Settings & forms to see where the run stopped.",
            false,
        ),
    };
    Failure {
        category,
        message,
        retryable,
    }
}

pub(super) async fn explain(db: &SqlitePool, task: &Task, failure: &Failure) -> Result<String> {
    let entries: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT e.kind,e.name,e.status,e.detail FROM tasks t
         JOIN audit_sessions s ON s.id=t.audit_session_id JOIN audit_entries e ON e.session=s.id
         WHERE t.id=? AND t.user=? AND t.revision=?
           AND ((e.kind='tool' AND e.status='completed' AND e.name IN
                    ('send_email','form_submission','send_task_file','browse','search_web','call_mcp','delegate','set_goal')) OR
                (e.kind='tool' AND e.status='failed' AND e.attempt=s.attempts) OR
                (e.kind='model' AND e.name IN ('output_limit_recovery','input_limit_recovery')
                    AND e.status='failed' AND e.attempt=s.attempts))
         ORDER BY e.id DESC LIMIT 32",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_all(db)
    .await?;
    let last_step = entries
        .iter()
        .filter(|(kind, _, status, _)| kind == "tool" && status == "completed")
        .find_map(|(_, name, _, raw)| {
            let detail = serde_json::from_str(raw).unwrap_or(Value::Null);
            confirmed_step(name, &detail)
        });
    let tool_problem = entries
        .iter()
        .filter(|(kind, _, status, _)| kind == "tool" && status == "failed")
        .find_map(|(_, _, _, raw)| {
            let detail: Value = serde_json::from_str(raw).ok()?;
            match detail["category"].as_str()? {
                "site_verification_required" => {
                    Some("A website asked for verification during the run.")
                }
                "https_connection_failed" => Some("A website connection failed during the run."),
                "worker_unavailable" => Some("A worker was unavailable during the run."),
                "timeout" => Some("One of the tools timed out during the run."),
                _ => None,
            }
        });
    let recovery_failed = match failure.category {
        "model_output_limit" => Some((
            "output_limit_recovery",
            "I tried finishing with a shorter reply, and the recovery attempt also stopped.",
        )),
        "model_context_limit" => Some((
            "input_limit_recovery",
            "I retried with less context, and the recovery attempt also stopped.",
        )),
        _ => None,
    }
    .and_then(|(name, text)| {
        entries
            .iter()
            .any(|(kind, entry, _, _)| kind == "model" && entry == name)
            .then_some(text)
    });

    Ok([
        Some(failure.message),
        last_step,
        tool_problem,
        recovery_failed,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" "))
}

fn confirmed_step(name: &str, detail: &Value) -> Option<&'static str> {
    match name {
        "send_email" => Some("The mail server had already accepted the email."),
        "form_submission" if detail["submitted"] == true => {
            Some("The website had already responded to the form submission.")
        }
        "send_task_file" if detail["queued"] == true => {
            Some("I had already queued a file for delivery.")
        }
        "set_goal" if detail["saved"] == true => Some("I had already saved the task goal."),
        "browse" if detail["page_bytes"].as_u64().is_some_and(|bytes| bytes > 0) => {
            Some("I had already read a web page for this task.")
        }
        "search_web"
            if detail["result_count"]
                .as_u64()
                .is_some_and(|count| count > 0) =>
        {
            Some("I had already found public search results.")
        }
        "call_mcp" => Some("The connected service had already returned a response."),
        "delegate" if detail["helper_replied"] == true => Some("A helper had already responded."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_details_use_a_closed_set_and_hide_raw_errors() {
        let source = "private test provider payload sk-or-fixture";
        for (failure, category, retryable) in [
            (
                ProviderFailure::OpenRouterKey,
                "openrouter_key_required",
                false,
            ),
            (ProviderFailure::RateLimited, "model_rate_limited", true),
            (ProviderFailure::Incomplete, "model_reply_incomplete", true),
            (ProviderFailure::Refused, "model_refused", false),
        ] {
            let classified = classify(&anyhow::Error::new(failure).context(source));
            assert_eq!(classified.category, category);
            assert_eq!(classified.retryable, retryable);
            assert!(!classified.message.contains(source));
        }
        let unknown = classify(&anyhow::anyhow!(source));
        assert_eq!(unknown.category, "task_failed");
        assert!(!unknown.message.contains(source));
        assert!(!unknown.retryable);
    }

    #[tokio::test]
    async fn explanation_uses_confirmed_steps_from_the_current_task_audit() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        sqlx::query("UPDATE users SET provider='codex' WHERE id='1'")
            .execute(&db)
            .await
            .unwrap();
        crate::store::ingest(&db, "1", "test", 1, "1", "Find public sources")
            .await
            .unwrap();
        let task = crate::store::claim(&db).await.unwrap().unwrap();
        let trace = crate::audit::Trace::start(&db, &task).await.unwrap();
        crate::audit::scope(trace, async {
            crate::audit::record(
                "tool",
                "search_web",
                "completed",
                serde_json::json!({"result_count":2,"fixture":"audit-fixture-private-data"}),
            )
            .await;
            crate::audit::record(
                "tool",
                "browse",
                "failed",
                serde_json::json!({"category":"timeout","fixture":"audit-fixture-private-data"}),
            )
            .await;
            crate::audit::record("model", "output_limit_recovery", "failed", Value::Null).await;
        })
        .await;
        let failure = classify(&ProviderFailure::OutputLimit.into());
        let text = explain(&db, &task, &failure).await.unwrap();
        assert!(text.contains("found public search results") && text.contains("shorter reply"));
        assert!(text.contains("timed out"));
        assert!(!text.contains("audit-fixture-private-data"));
        crate::audit::Trace::start(&db, &task).await.unwrap();
        let resumed = explain(&db, &task, &failure).await.unwrap();
        assert!(resumed.contains("found public search results"));
        assert!(!resumed.contains("timed out") && !resumed.contains("shorter reply"));
        let mut foreign = task;
        foreign.user = "2".into();
        assert_eq!(
            explain(&db, &foreign, &failure).await.unwrap(),
            failure.message
        );
    }
}
