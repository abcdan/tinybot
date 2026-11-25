use crate::{
    App, audit,
    model::{ProviderFailure, parse_json_reply},
    schedules::Preview,
    store::{self, Task},
};
use anyhow::{Result, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    text: String,
}

pub(super) async fn run(app: &App, task: &Task, preview: Preview) -> Result<()> {
    let model = app.models.provider(&task.user, false).await?;
    let input = serde_json::to_string(&preview)?;
    let reply = audit::model("routine_dry_run", async {
        let raw = model.complete_brief_reply(
            concat!(
                "Simulate this saved routine using only the synthetic sample and stored instructions supplied as data. ",
                "Show what each step would produce and where it would need a live service or review. ",
                "Never claim any tool, connector, helper or external action ran. Return JSON with one text field."
            ),
            &input,
        ).await?;
        let reply: Reply = parse_json_reply(&raw)?;
        ensure!(!reply.text.trim().is_empty() && reply.text.len() <= 8_000, ProviderFailure::Incomplete);
        Ok(reply.text)
    }).await?;
    if !store::current(&app.db, task).await? {
        return store::settle(&app.db, task, "idle", None).await;
    }
    store::settle_reply(&app.db, task, &format!("Dry run\n\n{reply}"), &[]).await
}
