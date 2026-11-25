use super::{Intent, Outcome, Risk, Tool, ToolContext};
use crate::{
    settings::{self, Key},
    store::settings_requests,
};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct ReadSettings;
pub(super) struct ConfigureSettings;

pub(super) fn is_settings_tool(name: &str) -> bool {
    matches!(name, "read_settings" | "configure_settings")
}

pub(super) async fn available(ctx: &ToolContext<'_>) -> Result<bool> {
    Ok(ctx.owner
        && ctx.parent_task.is_none()
        && ctx.task.agent.is_none()
        && ctx.task.shared_memory_pair.is_none()
        && settings_requests::permitted(&ctx.app.db, ctx.task).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    fields: Vec<Key>,
    #[serde(default)]
    values: BTreeMap<Key, String>,
}

impl Proposal {
    fn parse(args: &Value) -> Result<Self> {
        let proposal: Self = serde_json::from_value(args.clone())
            .map_err(|_| anyhow::anyhow!("Choose known personal settings and text values."))?;
        let requested: BTreeSet<_> = proposal.fields.iter().copied().collect();
        ensure!(
            !proposal.fields.is_empty()
                && proposal.fields.len() <= Key::ALL.len()
                && requested.len() == proposal.fields.len()
                && proposal
                    .values
                    .iter()
                    .all(|(key, value)| requested.contains(key) && value.len() <= 256),
            "Select distinct personal settings and prefill values only for those fields."
        );
        Ok(proposal)
    }
}

#[async_trait]
impl Tool for ReadSettings {
    fn name(&self) -> &'static str {
        "read_settings"
    }

    fn describe(&self) -> &'static str {
        "Read the current personal TinyBot settings and saved connection status for this account. Use this for questions about TinyBot's own configuration. Requires no arguments."
    }

    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            args.as_object().is_some_and(Map::is_empty),
            "This tool takes no arguments."
        );
        settings_intent(ctx, "Read personal settings").await
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        self.inspect(ctx, &args).await?;
        Ok(Outcome::Continue(
            settings::read(ctx.app, &ctx.task.user).await?,
        ))
    }
}

#[async_trait]
impl Tool for ConfigureSettings {
    fn name(&self) -> &'static str {
        "configure_settings"
    }

    fn describe(&self) -> &'static str {
        "Open a personal settings form on the user's request. Supply the specific fields they want to change and optionally prefill values explicitly given by the user. Use on or off for OpenRouter tools. Use codex or openrouter:publisher/model for task and backup models, or off for the backup model. For image_model, use a model ID from read_settings. The user reviews and approves the exact changes after the form. Never ask for authentication secrets or use this tool for access grants."
    }

    fn input_schema(&self) -> Value {
        let keys: Vec<_> = Key::ALL.into_iter().map(|key| key.as_str()).collect();
        let values: Map<String, Value> = keys
            .iter()
            .map(|key| (key.to_string(), json!({"type":"string","maxLength":256})))
            .collect();
        json!({
            "type":"object",
            "properties":{
                "fields":{"type":"array","minItems":1,"maxItems":Key::ALL.len(),"items":{"type":"string","enum":keys}},
                "values":{"type":"object","properties":values,"additionalProperties":false}
            },
            "required":["fields"],
            "additionalProperties":false
        })
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        Proposal::parse(args)?;
        settings_intent(ctx, "Open a personal settings form").await
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        settings_intent(ctx, "Open a personal settings form").await?;
        let Proposal { fields, values } = Proposal::parse(&args)?;
        settings_requests::propose(ctx.app, ctx.task, fields, values).await?;
        Ok(Outcome::Pause)
    }
}

async fn settings_intent(ctx: &ToolContext<'_>, detail: &str) -> Result<Intent> {
    ensure!(
        available(ctx).await?,
        "Use your personal Chief in a private chat to manage settings."
    );
    Ok(Intent {
        risk: Risk::Local,
        destination: "Your TinyBot account".into(),
        detail: detail.into(),
    })
}
