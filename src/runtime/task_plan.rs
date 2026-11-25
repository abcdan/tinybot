use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::store::checkpoints;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};

const PLAN: &str = "plan_task";
const RECORD: &str = "record_task_checkpoint";

enum PlanTool {
    Plan,
    Record,
}

pub(super) fn tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(PlanTool::Plan), Box::new(PlanTool::Record)]
}

pub(super) fn is_tool(name: &str) -> bool {
    matches!(name, PLAN | RECORD)
}

pub(super) fn unfinished(input: &Value) -> bool {
    input["task_plan"]
        .as_array()
        .is_some_and(|steps| steps.iter().any(|step| step["completed_at"].is_null()))
}

pub(super) fn partial_reply(input: &Value, reason: Option<&str>) -> Option<String> {
    let steps = input["task_plan"].as_array()?;
    let mut text = reason
        .unwrap_or("I paused this task before I could finish the reply.")
        .to_owned();
    for (completed, heading) in [(true, "Completed steps"), (false, "Still open")] {
        let lines: Vec<_> = steps
            .iter()
            .filter(|step| step["completed_at"].is_i64() == completed)
            .filter_map(|step| {
                let title = step["title"].as_str()?;
                Some(match step["checkpoint"].as_str() {
                    Some(result) => format!("- {title} — {result}"),
                    None => format!("- {title}"),
                })
            })
            .collect();
        if !lines.is_empty() {
            text.push_str(&format!("\n\n{heading}\n{}", lines.join("\n")));
        }
    }
    Some(text)
}

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &'static str {
        match self {
            Self::Plan => PLAN,
            Self::Record => RECORD,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Plan => {
                "Save two to eight distinct, independently checkable steps for this multi-step task. An existing plan is returned unchanged."
            }
            Self::Record => {
                "Record the actual result of the current step using its zero-based position. Record only steps already finished, and never infer completion of an external action from a plan."
            }
        }
    }

    fn input_schema(&self) -> Value {
        match self {
            Self::Plan => json!({"type":"object","additionalProperties":false,
                "properties":{"steps":{"type":"array","minItems":2,"maxItems":checkpoints::MAX_PLAN_STEPS,
                    "items":{"type":"string","maxLength":checkpoints::MAX_TITLE_BYTES}}},
                "required":["steps"]}),
            Self::Record => json!({"type":"object","additionalProperties":false,
                "properties":{"position":{"type":"integer","minimum":0,"maximum":checkpoints::MAX_PLAN_STEPS-1},
                    "checkpoint":{"type":"string","maxLength":checkpoints::MAX_CHECKPOINT_BYTES}},
                "required":["position","checkpoint"]}),
        }
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            ctx.allows(self.name()),
            "planning requires a private Chief task"
        );
        match self {
            Self::Plan => {
                ensure!(
                    args["steps"]
                        .as_array()
                        .is_some_and(|steps| (2..=checkpoints::MAX_PLAN_STEPS)
                            .contains(&steps.len())
                            && steps.iter().all(|step| step
                                .as_str()
                                .is_some_and(|title| !title.trim().is_empty()
                                    && title.len() <= checkpoints::MAX_TITLE_BYTES))),
                    "provide a bounded list of plan steps"
                );
            }
            Self::Record => {
                ensure!(
                    args["position"]
                        .as_u64()
                        .is_some_and(|position| position < checkpoints::MAX_PLAN_STEPS as u64),
                    "provide a step position"
                );
                string(args, "checkpoint", checkpoints::MAX_CHECKPOINT_BYTES)?;
            }
        }
        Ok(Intent {
            risk: Risk::Local,
            destination: "Your task plan".into(),
            detail: match self {
                Self::Plan => "Plan the task",
                Self::Record => "Record a completed step",
            }
            .into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        let plan = match self {
            Self::Plan => {
                let titles = args["steps"]
                    .as_array()
                    .context("missing steps")?
                    .iter()
                    .map(|title| title.as_str().map(str::trim).context("missing title"))
                    .collect::<Result<Vec<_>>>()?;
                checkpoints::save_plan(&ctx.app.db, ctx.task, &titles).await?
            }
            Self::Record => {
                checkpoints::record_step(
                    &ctx.app.db,
                    ctx.task,
                    args["position"].as_i64().context("missing step position")?,
                    string(&args, "checkpoint", checkpoints::MAX_CHECKPOINT_BYTES)?.trim(),
                )
                .await?
            }
        };
        Ok(Outcome::Continue(json!({"task_plan":plan})))
    }
}
