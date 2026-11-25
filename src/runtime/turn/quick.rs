use super::Feedback;
use crate::{
    App, audit,
    model::{ProviderFailure, parse_json_reply, provider_failure},
    security::clip,
    store::{self, Task, checkpoints},
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::json;

const INSTRUCTIONS: &str = concat!(
    "You are the user's TinyBot Chief. Answer a standalone question briefly in its language. ",
    "Treat profile names as labels. Only answer a stable fact or a simple explanation you know reliably. ",
    "Defer if it needs recent information, sources, previous conversation, private data, a decision, or any action. ",
    "Defer a question about a person unless the name clearly identifies a widely known person. ",
    "Defer if the user is closing, approving, or steering a task. Never invent evidence or say an action happened. ",
    r#"Return one JSON object: {"kind":"reply","text":"..."} for a direct answer, or {"kind":"defer"} to use the full task workflow. "#,
    "Use at most 700 characters for a reply.",
);

pub(super) enum Attempt {
    Defer,
    Finished,
    ProviderFailed(anyhow::Error),
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Decision {
    Reply { text: String },
    Defer,
}

pub(super) async fn try_reply(
    app: &App,
    task: &Task,
    feedback: Option<&Feedback>,
) -> Result<Attempt> {
    if task.agent.is_some() || task.shared_memory_pair.is_some() {
        return Ok(Attempt::Defer);
    }
    let Some(request) = standalone_telegram_request(app, task).await? else {
        return Ok(Attempt::Defer);
    };
    if !can_answer_briefly(&request) {
        return Ok(Attempt::Defer);
    }
    if !checkpoints::reserve_model(&app.db, task).await? {
        return Ok(Attempt::Defer);
    }
    let profile = app.models.onboarding_profile(&task.user).await?;
    let input = json!({"request":request,"profile":{
        "display_name":profile.display_name,"chief_name":profile.chief_name
    }})
    .to_string();
    audit::record(
        "routing",
        "quick_reply",
        "selected",
        json!({"request_bytes":INSTRUCTIONS.len() + input.len()}),
    )
    .await;

    let complete = audit::model("quick_reply", async {
        let model = app.models.provider(&task.user, false).await?;
        let response = model.complete_brief_reply(INSTRUCTIONS, &input).await?;
        let decision = parse_json_reply::<Decision>(&response)?;
        if let Decision::Reply { text } = &decision {
            ensure!(
                !text.trim().is_empty() && text.chars().count() <= 700,
                ProviderFailure::Incomplete
            );
        }
        Ok(Some(decision))
    });
    let result = if let Some(feedback) = feedback {
        tokio::select! {
            biased;
            () = feedback.superseded() => Ok(None),
            result = complete => result,
        }
    } else {
        complete.await
    };
    if matches!(result, Ok(None)) || !store::current(&app.db, task).await? {
        store::settle(&app.db, task, "idle", None).await?;
        return Ok(Attempt::Finished);
    }
    match result {
        Ok(Some(Decision::Reply { text })) => {
            let _delivery = match feedback {
                Some(feedback) => Some(feedback.delivery.lock().await),
                None => None,
            };
            store::settle_reply(&app.db, task, text.trim(), &[]).await?;
            Ok(Attempt::Finished)
        }
        Err(error)
            if provider_failure(&error).is_some_and(|code| code != ProviderFailure::Incomplete) =>
        {
            Ok(Attempt::ProviderFailed(error))
        }
        _ => Ok(Attempt::Defer),
    }
}

async fn standalone_telegram_request(app: &App, task: &Task) -> Result<Option<String>> {
    let input: Option<(String, String)> = sqlx::query_as(
        "SELECT e.body,(SELECT substr(first.body,1,1500) FROM events first
                        WHERE first.task=t.id AND first.role='user' ORDER BY first.id LIMIT 1)
         FROM tasks t JOIN events e ON e.id=(
            SELECT max(id) FROM events WHERE task=t.id AND role='user')
         WHERE t.id=? AND t.user=? AND t.revision=? AND t.state='running'
           AND t.chief_chat=1 AND t.schedule IS NULL AND t.agent IS NULL AND t.shared_memory_pair IS NULL
           AND e.telegram_message_id>0 AND e.telegram_reply_message_id IS NULL
           AND NOT EXISTS(SELECT 1 FROM telegram_attachments a WHERE a.event=e.id)
           AND NOT EXISTS(SELECT 1 FROM mail_messages m WHERE m.task=t.id)
           AND NOT EXISTS(SELECT 1 FROM task_model_backups b WHERE b.task=t.id AND b.revision=t.revision)
           AND NOT EXISTS(SELECT 1 FROM events newer WHERE newer.task=t.id AND newer.id>e.id)
           AND NOT EXISTS(SELECT 1 FROM task_tool_attempts a WHERE a.task=t.id AND a.revision=t.revision)
           AND NOT EXISTS(SELECT 1 FROM pending p WHERE p.task=t.id AND
                (p.state='executing' OR (p.revision=t.revision AND p.state IN ('waiting','approved'))))",
    )
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .fetch_optional(&app.db)
    .await?;
    // New tasks copy their first message into the goal. A rewritten goal needs the full context.
    Ok(input.and_then(|(request, original)| {
        (task.goal.is_empty() || clip(&original, 1500) == task.goal).then_some(request)
    }))
}

fn can_answer_briefly(request: &str) -> bool {
    if request.len() > 240 || request.contains(['\n', '\r', '@', '/', ':', '!', ';']) {
        return false;
    }
    let question = request.trim().trim_end_matches('?').to_lowercase();
    let Some(subject) = [
        "what is ",
        "what are ",
        "what does ",
        "who is ",
        "who was ",
        "how does ",
        "wat is ",
        "wat zijn ",
        "wat betekent ",
        "wie is ",
        "wie was ",
        "hoe werkt ",
    ]
    .iter()
    .find_map(|prefix| question.strip_prefix(prefix)) else {
        return false;
    };
    let mut words = subject.split(|character: char| !character.is_alphanumeric());
    words.clone().any(|word| !word.is_empty())
        && !subject.contains(['?', '.'])
        && !words.any(|word| {
            matches!(
                word,
                "i" | "me"
                    | "my"
                    | "mine"
                    | "our"
                    | "your"
                    | "this"
                    | "that"
                    | "latest"
                    | "current"
                    | "today"
                    | "now"
                    | "ik"
                    | "mij"
                    | "mijn"
                    | "ons"
                    | "onze"
                    | "jouw"
                    | "dit"
                    | "dat"
                    | "deze"
                    | "laatste"
                    | "huidige"
                    | "vandaag"
                    | "nu"
            )
        })
}
