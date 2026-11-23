use crate::{
    App, audit,
    store::{
        Task,
        tool_budget::{self, LIMIT},
    },
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};

pub(super) enum Progress {
    Proceed,
    Finish(&'static str),
    Superseded,
}

pub(super) async fn check(app: &App, task: &Task, request: &str) -> Result<Progress> {
    let Some(progress) = tool_budget::progress(&app.db, task).await? else {
        return Ok(Progress::Superseded);
    };
    let stalled = if let Some(batch) = progress.pending {
        let (state, repeated) = batch_input(batch.attempts.as_slice(), request);
        let decision = audit::model("tool_loop_check", async {
            tokio::time::timeout(
                Duration::from_secs(5),
                app.models.tool_loop_decision(&task.user, &state),
            )
            .await?
        })
        .await;
        let used_jev = matches!(decision, Ok(Some(_)));
        let stalled = decision.ok().flatten().unwrap_or(repeated);
        if !tool_budget::complete_check(&app.db, task, batch.position, stalled).await? {
            return Ok(Progress::Superseded);
        }
        audit::record(
            "tool_guard",
            "loop_check",
            if stalled { "stalled" } else { "continue" },
            json!({
                "calls":batch.position,"source":if used_jev {"jev"} else {"repeat_detection"}
            }),
        )
        .await;
        stalled
    } else {
        progress.stalled
    };
    Ok(if stalled {
        Progress::Finish("Recent tool calls were repeating without making progress.")
    } else if progress.calls >= LIMIT {
        Progress::Finish("This turn used its full tool budget.")
    } else {
        Progress::Proceed
    })
}

fn batch_input(attempts: &[tool_budget::ToolAttempt], request: &str) -> (Value, bool) {
    let mut targets = HashMap::new();
    let mut counts = HashMap::new();
    let calls: Vec<_> = attempts
        .iter()
        .map(|attempt| {
            // Give Jev equality signals without disclosing parameters or their fingerprints.
            let target = *targets
                .entry(attempt.args_fingerprint.as_str())
                .or_insert(attempt.position);
            *counts.entry(target).or_insert(0) += 1;
            json!({"actor":attempt.actor,"tool":attempt.tool,"target":target})
        })
        .collect();
    let repeated = counts.values().any(|count| *count >= 8)
        || (counts.len() == 2
            && calls
                .windows(3)
                .all(|window| window[0]["target"] == window[2]["target"]));
    (
        json!({"request":audit::request_preview(request),"attempts":calls}),
        repeated,
    )
}

pub(super) async fn reserve(
    app: &App,
    task: &Task,
    actor: &str,
    name: &str,
    args: &Value,
) -> Result<bool> {
    let name = if !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        name
    } else {
        "unknown_tool"
    };
    tool_budget::record(&app.db, task, actor, name, args).await
}
