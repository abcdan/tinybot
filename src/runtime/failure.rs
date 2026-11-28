use crate::{
    model::{ProviderFailure, provider_failure},
    store::Task,
};
use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

const INPUT_LIMIT: &str = "The conversation was too long for the model to process.";
const OUTPUT_LIMIT: &str = "The answer hit the model’s response limit.";

pub(super) struct Failure {
    pub category: &'static str,
    pub message: &'static str,
    pub retryable: bool,
}

pub(super) fn classify(error: &anyhow::Error) -> Failure {
    let provider = provider_failure(error);
    let (message, retryable) = match provider {
        Some(ProviderFailure::Setup) => (
            "This task needs a model. Open Settings & forms → Model to choose one.",
            false,
        ),
        Some(ProviderFailure::OpenRouterKey) => (
            "OpenRouter needs a working key. Open Settings & forms → Model to check or replace it.",
            false,
        ),
        Some(ProviderFailure::OpenRouterCredits) => (
            "OpenRouter reported a credit or spending limit. Check your OpenRouter balance and key limits.",
            false,
        ),
        Some(ProviderFailure::CodexSignIn) => (
            "The ChatGPT connection needs to be reauthorized. Open Settings & forms → Model to reconnect it.",
            false,
        ),
        Some(ProviderFailure::CodexService) => (
            "The Codex service on this TinyBot server is unavailable. Choose another connected model or ask the bot operator to check it.",
            false,
        ),
        Some(ProviderFailure::BackupDisabled) => (
            "The backup model is no longer enabled. Check your task and backup models in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::ModelUnavailable) => (
            "The selected model is unavailable for this connection. Choose another model in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::RateLimited) => (
            "The model provider is limiting requests right now. Give it a moment.",
            true,
        ),
        Some(ProviderFailure::TimedOut) => (
            "The model took too long to answer. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Temporary) => (
            "The model provider is temporarily unavailable. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Incomplete) => (
            "The model didn’t return a usable reply. If it keeps happening, choose another task model in Settings & forms.",
            true,
        ),
        Some(ProviderFailure::InputLimit) => (INPUT_LIMIT, false),
        Some(ProviderFailure::OutputLimit) => (OUTPUT_LIMIT, false),
        Some(ProviderFailure::Refused) => (
            "The model declined this request. Send a different request to continue.",
            false,
        ),
        Some(ProviderFailure::AccessDenied) => (
            "The model provider blocked this request. Check the permissions for your connected account in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::Rejected) => (
            "The model provider rejected this request. Check that your selected model supports tools, or choose another model in Settings & forms.",
            false,
        ),
        None => (
            "I ran into a problem and paused the task. Open Activity in the Mini App to see what happened.",
            false,
        ),
    };
    Failure {
        category: provider.map_or("task_failed", ProviderFailure::category),
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
                    ('send_email','form_submission','send_task_file','browse','search_web','call_mcp','delegate','set_goal','react_to_message')) OR
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
    let recovery =
        entries
            .iter()
            .find(|(kind, _, _, _)| kind == "model")
            .map(|(_, name, _, raw)| {
                let detail: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
                let timed_out = detail["category"].as_str().unwrap_or(failure.category)
                    == ProviderFailure::TimedOut.category();
                let (cause, explanation) = match (name.as_str(), timed_out) {
                    ("output_limit_recovery", true) => (
                        OUTPUT_LIMIT,
                        "My attempt to finish with a shorter reply timed out.",
                    ),
                    ("output_limit_recovery", false) => (
                        OUTPUT_LIMIT,
                        "I tried finishing with a shorter reply. That attempt also stopped.",
                    ),
                    (_, true) => (INPUT_LIMIT, "My retry with less context timed out."),
                    (_, false) => (
                        INPUT_LIMIT,
                        "I retried with less context. That attempt also stopped.",
                    ),
                };
                (cause, explanation, timed_out)
            });
    let (message, recovery_failed, outcome) = match recovery {
        Some((cause, explanation, timed_out)) => {
            let outcome = if timed_out && failure.category == ProviderFailure::TimedOut.category() {
                Some("Your task is saved.")
            } else {
                (failure.message != cause).then_some(failure.message)
            };
            (cause, Some(explanation), outcome)
        }
        None => (failure.message, None, None),
    };

    Ok([
        Some(message),
        recovery_failed,
        last_step,
        tool_problem,
        outcome,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" "))
}

pub(crate) fn confirmed_step(name: &str, detail: &Value) -> Option<&'static str> {
    match name {
        "send_email" => Some("The mail server had already accepted the email."),
        "form_submission" if detail["submitted"] == true => {
            Some("The website had already responded to the form submission.")
        }
        "send_task_file" if detail["queued"] == true => {
            Some("I had already queued a file for delivery.")
        }
        "react_to_message" if detail["queued"] == true => {
            Some("I had already queued a Telegram reaction.")
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

    #[tokio::test]
    async fn a_timed_out_limit_retry_explains_both_failures() {
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
        crate::store::ingest(&db, "1", "test", 1, "1", "Short follow-up")
            .await
            .unwrap();
        let task = crate::store::claim(&db).await.unwrap().unwrap();
        let trace = crate::audit::Trace::start(&db, &task).await.unwrap();
        crate::audit::scope(trace, async {
            let _: Result<()> = crate::audit::model("agent_turn_stream", async {
                Err(ProviderFailure::OutputLimit.into())
            })
            .await;
            let _: Result<()> = crate::audit::model("output_limit_recovery", async {
                Err(anyhow::Error::new(ProviderFailure::TimedOut)
                    .context("private provider detail"))
            })
            .await;
        })
        .await;

        let failed_steps: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, json_extract(detail,'$.category') FROM audit_entries
             WHERE kind='model' AND status='failed' ORDER BY id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            failed_steps,
            [
                (
                    "agent_turn_stream".to_owned(),
                    "model_output_limit".to_owned()
                ),
                (
                    "output_limit_recovery".to_owned(),
                    "model_timed_out".to_owned()
                ),
            ]
        );
        let failure = classify(&ProviderFailure::TimedOut.into());
        let notice = explain(&db, &task, &failure).await.unwrap();
        assert_eq!(
            notice,
            format!(
                "{OUTPUT_LIMIT} My attempt to finish with a shorter reply timed out. Your task is saved."
            )
        );
    }
}
