use super::email;
use super::execution::{ApprovalProgress, finish, resume_approval};
use super::feedback::Feedback;
use super::lookup::{self, Lookup};
use super::loop_guard::{self, Progress};
use super::mcp_approval;
use super::review::{Verdict, review};
use super::suggestions::valid_suggestions;
use super::task_plan;
use super::{
    Call, ENGINE, Outcome, Risk, Step, ToolContext, connector_context, descriptions,
    tool_definitions,
};
use crate::{
    App, audit,
    model::{ModelProvider, ProviderFailure, provider_failure, temporary_provider_failure},
    schedules,
    security::{clip, id, now},
    store::{self, Agent, Task, checkpoints},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

mod history;
mod limits;
mod quick;
mod task_list;

const ACTIVE_TURN_BUDGET: Duration = Duration::from_secs(8 * 60);

pub(super) async fn work_task(app: &App, task: &Task, feedback: Option<&Feedback>) -> Result<()> {
    let mut compaction = None;
    run_turn(app, task, feedback, &mut compaction).await?;
    if let Some(compaction) = compaction {
        compaction.schedule(&app.db, task);
    }
    Ok(())
}

async fn run_turn(
    app: &App,
    task: &Task,
    feedback: Option<&Feedback>,
    compaction: &mut Option<history::Compaction>,
) -> Result<()> {
    if store::groups::is_group_task(&app.db, &task.id).await? {
        return super::group::work(app, task).await;
    }
    if !store::current(&app.db, task).await? {
        return store::settle(&app.db, task, "cancelled", None).await;
    }
    let started = Instant::now();
    let mut quick_failure = match quick::try_reply(app, task, feedback).await? {
        quick::Attempt::Finished => return Ok(()),
        quick::Attempt::Defer => None,
        quick::Attempt::ProviderFailed(error) => Some(error),
    };
    let paired = task.shared_memory_pair.is_some();
    let agent = if let Some(agent) = &task.agent {
        sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE id=?")
            .bind(agent)
            .fetch_optional(&app.db)
            .await?
    } else {
        None
    };
    let owner = agent.as_ref().is_none_or(|agent| agent.owner == task.user);
    let connectors = if paired {
        Vec::new()
    } else {
        app.mcp.available(&task.user, task.agent.as_deref()).await?
    };
    let allowed_tools = agent
        .as_ref()
        .map(|agent| serde_json::from_str::<Vec<String>>(&agent.tools))
        .transpose()?;
    let ctx = ToolContext {
        app,
        task,
        parent_task: None,
        owner,
        connectors,
        allowed_tools,
    };
    match resume_approval(&ctx).await? {
        ApprovalProgress::Continue => {}
        ApprovalProgress::Settle => return store::settle(&app.db, task, "idle", None).await,
        ApprovalProgress::Finished => return Ok(()),
    }
    let saved: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT a.id,a.name,CASE WHEN a.owner=? THEN a.instructions END FROM agents a
         WHERE (a.owner=? OR EXISTS(SELECT 1 FROM grants g WHERE g.agent=a.id AND g.member=?))
           AND (? IS NULL OR a.id=?)
         ORDER BY a.name LIMIT 30",
    )
    .bind(&task.user)
    .bind(&task.user)
    .bind(&task.user)
    .bind(&task.agent)
    .bind(&task.agent)
    .fetch_all(&app.db)
    .await?;
    let saved: Vec<_> = saved
        .iter()
        .map(|(id, name, description)| {
            json!({
                "id":id,"name":name,"description":description.as_deref().map(|text|clip(text,500))
            })
        })
        .collect();
    let mut backup = store::model_backup::selected(&app.db, task).await?;
    let mut model = if let Some(selected) = &backup {
        app.models.backup_provider(&task.user, selected).await?
    } else {
        app.models.provider(&task.user, false).await?
    };
    let profile = app.models.onboarding_profile(&task.user).await?;
    let profile = json!({"display_name":profile.display_name,"chief_name":profile.chief_name});
    let incoming_mail = if paired {
        None
    } else {
        store::mail::task_context(&app.db, &app.vault, task).await?
    };
    let automatic_mail = incoming_mail
        .as_ref()
        .is_some_and(|mail| mail.automated_turn);
    let routine = schedules::progress(&app.db, task).await?;
    let inbox_routine = routine
        .as_ref()
        .and_then(|progress| progress.source.as_deref())
        == Some("inbox");
    let inbox_briefing = if inbox_routine {
        Some(store::mail::briefing(&app.db, &app.vault, task).await?)
    } else {
        None
    };
    let mail_accounts: Vec<_> =
        if owner && task.agent.is_none() && !paired && !inbox_routine {
            store::mail::smtp_accounts(&app.db, &task.user).await?.into_iter().map(|account| {
            json!({"id":account.id,"label":account.label,"address":account.address})
        }).collect()
        } else {
            Vec::new()
        };
    let hosted = if automatic_mail || paired || inbox_routine || backup.is_some() {
        Vec::new()
    } else {
        super::openrouter_tools::available(&ctx).await?
    };
    let personal_settings = super::settings::available(&ctx).await?;
    let calendar = if personal_settings {
        super::calendar::model_context(app, &task.user).await?
    } else {
        None
    };
    let can_plan = !paired
        && owner
        && task.agent.is_none()
        && !automatic_mail
        && checkpoints::can_plan(&app.db, task).await?;
    let image_tools_available =
        personal_settings && app.models.image_model_available(&task.user).await?;
    let mut tools: Vec<_> = app
        .tools
        .iter()
        .filter(|tool| {
            ctx.allows(tool.name())
                && (!super::media_tools::is_media_tool(tool.name()) || personal_settings)
                && (!super::media_tools::is_image_tool(tool.name()) || image_tools_available)
                && (!super::settings::is_settings_tool(tool.name()) || personal_settings)
                && (!matches!(
                    tool.name(),
                    super::calendar::CALENDAR_READ | super::calendar::CALENDAR_DRAFT
                ) || calendar.is_some())
                && (!task_plan::is_tool(tool.name()) || can_plan)
                && (!paired
                    || matches!(tool.name(), "send_reply" | "send_card" | "react_to_message"))
                && (!tool.name().starts_with("openrouter_") || hosted.contains(&tool.name()))
                && (owner || matches!(tool.name(), "delegate" | "send_reply" | "react_to_message"))
                && (tool.name() != "send_email" || !mail_accounts.is_empty())
                && (tool.name() != "record_routine_step" || routine.is_some())
                && (routine.is_none()
                    || !matches!(
                        tool.name(),
                        "schedule"
                            | "save_routine"
                            | "run_routine"
                            | "cancel_schedule"
                            | "delegate"
                    ))
                && (!inbox_routine
                    || matches!(
                        tool.name(),
                        "record_routine_step" | "send_reply" | "send_card" | "request_input"
                    ))
                && (!automatic_mail
                    || matches!(
                        tool.name(),
                        "send_reply" | "send_card" | "request_input" | "send_email"
                    ))
        })
        .collect();
    let latest_request: Option<(i64, String, Option<i64>)> = sqlx::query_as(
        "SELECT id,body,telegram_message_id FROM events WHERE task=? AND role='user' ORDER BY id DESC LIMIT 1",
    )
    .bind(&task.id)
    .fetch_optional(&app.db)
    .await?;
    let request = latest_request
        .as_ref()
        .map_or("", |(_, text, _)| text.as_str());
    let document_format = tools
        .iter()
        .any(|tool| tool.name() == super::output::CREATE_DOCUMENT)
        .then(|| super::output::requested_native_format(request))
        .flatten();
    if document_format.is_some() {
        tools.retain(|tool| tool.name() != "send_task_file");
    }
    let mut definitions = tool_definitions(&tools);
    let memory = if paired {
        store::shared_memory::context(&app.db, task).await?
    } else {
        super::memory::for_chief(
            app,
            task,
            request,
            owner
                && latest_request
                    .as_ref()
                    .is_some_and(|(_, _, id)| id.is_some_and(|id| id > 0)),
        )
        .await?
    };
    let mut attachment = if paired {
        Value::Null
    } else {
        super::media::context(
            app,
            task,
            &*model,
            latest_request.as_ref().map(|(_, text, _)| text.as_str()),
        )
        .await?
    };
    let mut goal = task.goal.clone();
    let mut lookups: BTreeSet<Lookup> = BTreeSet::new();
    let mut reads = if can_plan {
        checkpoints::read_cache(&app.db, &app.vault, task).await?
    } else {
        checkpoints::ReadCache::default()
    };
    let mut opened_page = reads.evidence.iter().find_map(lookup::opened_page);
    let mut consulted_helper = false;
    let mut repeated_lookup = false;
    let mut finish_turn = false;
    let mut goal_saved = false;
    let mut repeated_goal = false;
    let mut asked_for_step = false;
    let mut corrected_document = false;
    let mut recovery: Option<String> = None;
    loop {
        let progress = loop_guard::check(app, task, request).await?;
        if matches!(progress, Progress::Superseded) {
            return store::settle(&app.db, task, "idle", None).await;
        }
        let stop_reason = match progress {
            Progress::Finish(reason) => Some(reason),
            _ if started.elapsed() >= ACTIVE_TURN_BUDGET => {
                Some("This turn reached its active time budget.")
            }
            _ if quick_failure.is_none() && !checkpoints::reserve_model(&app.db, task).await? => {
                Some("This turn reached its model pass budget.")
            }
            _ => None,
        };
        let records = store::history(&app.db, &task.id).await?;
        let missing_document = document_format.filter(|format| {
            latest_request.as_ref().is_some_and(|(event, _, _)| {
                !records
                    .iter()
                    .rev()
                    .take_while(|(id, _, _)| id > event)
                    .any(|(_, role, body)| {
                        role == "tool" && super::output::queued_document(body, format)
                    })
            })
        });
        if !goal_saved
            && latest_request.as_ref().is_some_and(|(event, _, _)| {
                records
                    .iter()
                    .rev()
                    .take_while(|(id, _, _)| id > event)
                    .any(|(_, role, body)| {
                        role == "tool"
                            && serde_json::from_str::<Value>(body).is_ok_and(|result| {
                                result["tool"] == "set_goal" && result["saved"] == true
                            })
                    })
            })
        {
            goal_saved = true;
            tools.retain(|tool| tool.name() != "set_goal");
            definitions = tool_definitions(&tools);
        }
        if let Some((event, _, _)) = &latest_request {
            consulted_helper |= records
                .iter()
                .rev()
                .take_while(|(id, _, _)| id > event)
                .any(|(_, role, _)| role == "handoff");
        }
        let has_work = consulted_helper
            || !lookups.is_empty()
            || !reads.keys.is_empty()
            || records
                .iter()
                .rev()
                .take_while(|(id, _, _)| {
                    latest_request
                        .as_ref()
                        .is_none_or(|(latest, _, _)| id > latest)
                })
                .any(|(_, role, _)| role == "tool");
        if compaction.is_none() && !has_work && stop_reason.is_none() {
            *compaction = history::Compaction::for_history(&records, &model);
        }
        let history = history::prompt(&records, request);
        let zone = schedules::timezone(&app.db, &task.user).await?;
        let mut input = json!({
            "goal":goal,"summary":task.summary,"latest_request":latest_request.as_ref().map(|(_,text,_)|clip(text,8000)),"history":history,"memory":memory,"attachment":attachment,
            "profile":profile,
            "local_time":schedules::local_iso(now(),zone),"time_zone":zone.name(),
            "shared_helper_scope":agent.as_ref().filter(|_|!owner).map(|agent|&agent.id),
            "shared_memory_chat":paired,
            "telegram_reply":store::latest_reply_context(&app.db, &task.id).await?,
            "saved_agents":if paired || inbox_routine { &[][..] } else { &saved[..] },"tools":descriptions(&tools),
            "connectors":if automatic_mail || paired || inbox_routine { Vec::new() } else { connector_context(&ctx) },
            "email":incoming_mail,"mail_accounts":mail_accounts
        });
        if let Some(format) = missing_document {
            input["requested_document_format"] = json!(format);
        }
        let routine_progress = if routine.is_some() {
            schedules::progress(&app.db, task).await?
        } else {
            None
        };
        if let Some(progress) = &routine_progress {
            input["routine"] = serde_json::to_value(progress)?;
        }
        if let Some(calendar) = &calendar {
            input["calendar"] = calendar.clone();
        }
        if let Some(briefing) = &inbox_briefing {
            input["inbox_briefing"] = serde_json::to_value(briefing)?;
        }
        if can_plan {
            let plan = checkpoints::plan(&app.db, &task.id, task.revision, false).await?;
            if !plan.is_empty() {
                input["task_plan"] = serde_json::to_value(plan)?;
            }
        }
        if !reads.evidence.is_empty() {
            input["verified_reads"] = json!(reads.evidence);
        }
        if finish_turn || stop_reason.is_some() {
            if let Some(format) = missing_document {
                return super::output::missing_document(&ctx, format).await;
            }
            if let Some(page) = &opened_page {
                input["opened_page"] = page.clone();
            }
            return finish_from_context(app, task, feedback, &*model, &input, stop_reason).await;
        }
        let instructions = recovery.as_deref().unwrap_or(ENGINE);
        let compact_input = recovery.as_ref().map(|_| limits::replan_input(&input));
        let model_input = compact_input.as_ref().unwrap_or(&input);
        let step = if let Some(error) = quick_failure.take() {
            Err(error)
        } else if let Some(feedback) = feedback {
            feedback
                .complete(&*model, instructions, model_input, &definitions)
                .await
        } else {
            audit::model("agent_turn", async {
                let response = model
                    .complete_turn(instructions, &model_input.to_string(), &definitions, None)
                    .await?;
                super::reply::turn(&response).await.map(Some)
            })
            .await
        };
        if let Some(page) = &opened_page {
            input["opened_page"] = page.clone();
        }
        let step = match step {
            Err(error) => match limits::Limit::from_error(&error) {
                Some(limit) => {
                    let replan =
                        recovery.is_none() && !has_work && limits::can_replan(app, task).await?;
                    let instructions = recovery.get_or_insert_with(limits::replan_instructions);
                    let next = if replan {
                        limit
                            .replan(&*model, instructions, &input, &definitions, feedback)
                            .await
                    } else {
                        Ok(None)
                    };
                    match next {
                        Ok(Some(step)) => Ok(Some(step)),
                        Ok(None) if missing_document.is_some() => {
                            return super::output::missing_document(
                                &ctx,
                                missing_document.unwrap(),
                            )
                            .await;
                        }
                        Ok(None) => match limit
                            .finish(app, task, feedback, &*model, &input, error)
                            .await
                        {
                            Ok(()) => return Ok(()),
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    }
                }
                None => Err(error),
            },
            step => step,
        };
        let step = match step {
            Ok(Some(step)) => step,
            Ok(None) => return store::settle(&app.db, task, "idle", None).await,
            Err(error) => {
                if backup.is_none()
                    && temporary_provider_failure(&error)
                    && let Some(selected) = store::model_backup::begin(&app.db, task).await?
                {
                    audit::record(
                        "model",
                        "model_backup",
                        "switched",
                        json!({"provider":selected.provider,"model":selected.model}),
                    )
                    .await;
                    model = app.models.backup_provider(&task.user, &selected).await?;
                    backup = Some(selected);
                    if !store::current(&app.db, task).await? {
                        return store::settle(&app.db, task, "idle", None).await;
                    }
                    tools.retain(|tool| !tool.name().starts_with("openrouter_"));
                    definitions = tool_definitions(&tools);
                    attachment = super::media::context(app, task, &*model, Some(request)).await?;
                    continue;
                }
                if has_work
                    && provider_failure(&error)
                        .is_none_or(|failure| failure == ProviderFailure::Incomplete)
                {
                    if let Some(format) = missing_document {
                        return super::output::missing_document(&ctx, format).await;
                    }
                    return finish_from_context(app, task, feedback, &*model, &input, None).await;
                }
                return Err(error);
            }
        };
        if !store::current(&app.db, task).await? {
            return store::settle(&app.db, task, "idle", None).await;
        }
        if let Some(format) = missing_document
            && match &step {
                Step::Reply { text, .. } => !super::output::document_question(text),
                Step::Tool { name, args } => {
                    matches!(name.as_str(), "send_task_file" | "send_reply" | "send_card")
                        || (name == super::output::CREATE_DOCUMENT && args["format"] != format)
                }
            }
        {
            if corrected_document {
                return super::output::missing_document(&ctx, format).await;
            }
            corrected_document = true;
            store::append(
                &app.db,
                &task.id,
                "tool",
                &super::output::document_correction(format),
            )
            .await?;
            continue;
        }
        match step {
            Step::Reply {
                text,
                suggested_replies,
            } => {
                if let Some(progress) =
                    routine_progress.filter(|progress| progress.current.is_some())
                {
                    if asked_for_step {
                        return store::settle(
                            &app.db,
                            task,
                            "blocked",
                            Some(&format!(
                                "I finished {} of {} routine steps. Reply here to continue.",
                                progress.results.len(),
                                progress.total,
                            )),
                        )
                        .await;
                    }
                    asked_for_step = true;
                    store::append(
                        &app.db,
                        &task.id,
                        "tool",
                        "Complete the current routine step before replying. If you need a decision, use request_input.",
                    )
                    .await?;
                    continue;
                }
                let suggestions = valid_suggestions(&suggested_replies);
                audit::record(
                    "model",
                    "agent_turn",
                    "reply",
                    json!({"text_bytes":text.len(),"suggested_replies":suggestions.len()}),
                )
                .await;
                if text.trim().is_empty() {
                    if consulted_helper {
                        return finish_from_context(app, task, feedback, &*model, &input, None)
                            .await;
                    }
                    return store::settle(&app.db, task, "idle", None).await;
                }
                return super::routine_reply::send(app, task, &text, &suggestions, feedback).await;
            }
            Step::Tool { mut name, mut args } => {
                if name == "set_goal" && goal_saved {
                    audit::record(
                        "tool",
                        "set_goal",
                        "skipped",
                        json!({"category":"goal_already_saved"}),
                    )
                    .await;
                    if repeated_goal {
                        finish_turn = true;
                    } else {
                        repeated_goal = true;
                        store::append(
                            &app.db,
                            &task.id,
                            "tool",
                            "The task goal is already saved. Continue the requested work.",
                        )
                        .await?;
                    }
                    continue;
                }
                repeated_goal = false;
                if let Some(url) = lookup::supplied_url_query(&name, &args, request)
                    && tools.iter().any(|tool| tool.name() == "browse")
                    && tools.iter().any(|tool| tool.name() == name)
                    && !lookups.iter().any(|lookup| lookup.reads_page(&url))
                    && !reads.keys.contains(&Lookup::browse_key(&url))
                {
                    audit::record("tool", "browse", "selected", json!({"source":"user_link"}))
                        .await;
                    name = "browse".into();
                    args = json!({"url":url});
                }
                mcp_approval::prepare_docblock_call(&ctx, &name, &mut args);
                if !loop_guard::reserve(app, task, "chief", &name, &args).await? {
                    continue;
                }
                let Some(tool) = tools.iter().find(|tool| tool.name() == name) else {
                    audit::record("model", "agent_turn", "unknown_tool", Value::Null).await;
                    store::append(&app.db, &task.id, "tool", "Requested tool is unavailable.")
                        .await?;
                    continue;
                };
                super::media_tools::pin_image_model(&ctx, &name, &mut args).await?;
                audit::record(
                    "model",
                    "agent_turn",
                    "proposed_tool",
                    json!({"tool":tool.name()}),
                )
                .await;
                let intent = match tool.inspect(&ctx, &args).await {
                    Ok(intent) => intent,
                    Err(error) => {
                        audit::record(
                            "tool",
                            tool.name(),
                            "rejected",
                            json!({"category":"invalid_arguments_or_permissions"}),
                        )
                        .await;
                        store::append(&app.db, &task.id, "tool", clip(&error.to_string(), 500))
                            .await?;
                        continue;
                    }
                };
                let lookup = if intent.risk == Risk::PublicRead {
                    Lookup::new(&name, &args)?
                } else {
                    None
                };
                let saved_read = match lookup.as_ref().and_then(Lookup::cache_key) {
                    Some(key) if reads.keys.contains(&key) => {
                        checkpoints::cached_read(&app.db, &app.vault, task, &key).await?
                    }
                    _ => None,
                };
                if let Some(evidence) = &saved_read {
                    if let Some(page) = lookup::opened_page(evidence) {
                        opened_page = Some(page);
                    }
                    if !reads.evidence.contains(evidence) {
                        reads.evidence.insert(0, evidence.clone());
                        reads.evidence.truncate(3);
                    }
                }
                if lookup
                    .as_ref()
                    .is_some_and(|lookup| lookups.contains(lookup))
                    || saved_read.is_some()
                {
                    audit::record(
                        "tool",
                        tool.name(),
                        "skipped",
                        json!({"category":"duplicate_lookup","verified_checkpoint":saved_read.is_some()}),
                    )
                    .await;
                    if repeated_lookup {
                        finish_turn = true;
                    } else {
                        repeated_lookup = true;
                        store::append(&app.db, &task.id, "tool",
                            "That exact lookup already ran in this turn. Use its recorded result if relevant. If it did not answer the question, try a different public source or query that matches the user's request. Otherwise write the final answer.",
                        ).await?;
                    }
                    continue;
                }
                repeated_lookup = false;
                if name == "send_email" {
                    match email::request(&ctx, &args, incoming_mail.as_ref()).await {
                        Ok(true) => {
                            audit::record("tool", tool.name(), "waiting_approval", Value::Null)
                                .await;
                            return Ok(());
                        }
                        Ok(false) => {}
                        Err(error) => {
                            if !store::current(&app.db, task).await? {
                                return store::settle(&app.db, task, "idle", None).await;
                            }
                            audit::record("tool", tool.name(), "draft_rejected", Value::Null).await;
                            let reason = error.downcast_ref::<email::DraftError>().map_or(
                                "The email draft could not be saved. Nothing was sent.",
                                |error| error.0,
                            );
                            store::append(&app.db, &task.id, "tool", reason).await?;
                        }
                    }
                    continue;
                }
                let mcp = if name == "call_mcp" && ctx.owner {
                    let scope = mcp_approval::scope(&ctx, &args)?;
                    if store::mcp_denied_in_turn(&app.db, task, &scope, &args["arguments"]).await? {
                        audit::record(
                            "tool",
                            tool.name(),
                            "rejected",
                            json!({"category":"user_denied"}),
                        )
                        .await;
                        store::append(
                            &app.db,
                            &task.id,
                            "tool",
                            "The user denied this MCP call. Do not retry it in this turn.",
                        )
                        .await?;
                        continue;
                    }
                    Some(scope)
                } else {
                    None
                };
                let mut requires_confirmation = intent.risk == Risk::Write;
                if intent.risk != Risk::Local {
                    let review = if mcp.is_some() {
                        mcp_approval::review_call(&ctx, &intent, &args, request).await
                    } else {
                        review(&ctx, &intent).await
                    }
                    .context("decision model did not provide a valid review")?;
                    if !store::current(&app.db, task).await? {
                        return store::settle(&app.db, task, "idle", None).await;
                    }
                    if review.verdict == Verdict::Deny {
                        audit::record(
                            "tool",
                            tool.name(),
                            "rejected",
                            json!({"category":"decision_denied"}),
                        )
                        .await;
                        return store::settle(
                            &app.db,
                            task,
                            "blocked",
                            Some(&format!(
                                "I paused before {} at {}. {}",
                                intent.detail, intent.destination, review.reason
                            )),
                        )
                        .await;
                    }
                    requires_confirmation |= review.verdict == Verdict::Confirm;
                }
                if requires_confirmation {
                    ensure!(owner, "shared-agent members cannot approve this action");
                    let call = Call { name, args };
                    if let Some(scope) = mcp {
                        if !ctx.still_allows(tool.name()).await? {
                            return store::settle(&app.db, task, "idle", None).await;
                        }
                        tool.inspect(&ctx, &call.args).await?;
                        let action = id();
                        if store::start_remembered_mcp(&app.db, task, &action, &scope).await? {
                            audit::record(
                                "approval",
                                "mcp_call",
                                "remembered",
                                json!({"tool":scope.tool}),
                            )
                            .await;
                            audit::tool_started(tool.name(), &call.args).await;
                            let result = tool.execute(&ctx, call.args).await;
                            audit::tool_finished(tool.name(), &result).await;
                            let uncertain = result.is_err();
                            finish(&ctx, &action, result, uncertain).await?;
                            if uncertain {
                                return Ok(());
                            }
                            continue;
                        }
                        mcp_approval::pause(&ctx, &*model, &intent, call, &scope, request).await?;
                        audit::record("tool", tool.name(), "waiting_approval", Value::Null).await;
                        return Ok(());
                    }
                    let payload = json!({"call":call,"destination":intent.destination,"operation":intent.detail});
                    store::pause(
                        &app.db,
                        task,
                        "approve",
                        &payload,
                        &format!(
                            "Review {} at {} in the Mini App before I continue.",
                            intent.detail, intent.destination
                        ),
                        Some("mini"),
                        900,
                        &id(),
                    )
                    .await?;
                    audit::record("tool", tool.name(), "waiting_approval", Value::Null).await;
                    return Ok(());
                }
                if !ctx.still_allows(tool.name()).await? {
                    return store::settle(&app.db, task, "idle", None).await;
                }
                tool.inspect(&ctx, &args).await?;
                if name == "delegate" && bind_shared_delegation(&app.db, task, &args).await? {
                    audit::record("delegation", "helper", "scoped", Value::Null).await;
                    return Ok(());
                }
                // These operations manage their own durable task state.
                if task_plan::is_tool(&name)
                    || matches!(
                        name.as_str(),
                        "request_form"
                            | super::calendar::CALENDAR_DRAFT
                            | "request_input"
                            | "configure_settings"
                            | "complete_task"
                            | "propose_agent"
                            | "delegate"
                            | "connect_mcp"
                            | "send_reply"
                            | "send_card"
                    )
                {
                    audit::tool_started(tool.name(), &args).await;
                    let result = tool.execute(&ctx, args).await;
                    audit::tool_finished(tool.name(), &result).await;
                    match result {
                        Ok(Outcome::Pause) => return Ok(()),
                        Ok(Outcome::Media(_)) => {
                            anyhow::bail!("media requires an action checkpoint")
                        }
                        Ok(Outcome::Continue(value)) => {
                            store::append(&app.db, &task.id, "tool", &value.to_string()).await?;
                            consulted_helper |= name == "delegate";
                        }
                        Err(error) => {
                            if !store::current(&app.db, task).await? {
                                return store::settle(&app.db, task, "idle", None).await;
                            }
                            let reason = error.to_string();
                            tracing::warn!(task=%task.id, tool=%name, "interactive tool did not complete");
                            store::append(
                                &app.db,
                                &task.id,
                                "tool",
                                &format!(
                                    "The {name} tool did not complete. {}",
                                    clip(&reason, 360)
                                ),
                            )
                            .await?;
                        }
                    }
                    continue;
                }
                let action = id();
                if !store::start_action(&app.db, task, &action).await? {
                    return store::settle(&app.db, task, "idle", None).await;
                }
                audit::tool_started(tool.name(), &args).await;
                let execution = tool.execute(&ctx, args);
                let media = super::media_tools::is_media_tool(&name);
                let result = if let Some(feedback) = feedback
                    && media
                {
                    tokio::select! {
                        biased;
                        () = feedback.superseded() => None,
                        result = execution => Some(result),
                    }
                } else {
                    Some(execution.await)
                };
                if media && (result.is_none() || !store::current(&app.db, task).await?) {
                    store::interrupt_action(&app.db, task, &action).await?;
                    audit::record("tool", tool.name(), "superseded", Value::Null).await;
                    return store::settle(&app.db, task, "idle", None).await;
                }
                let result = result.context("media execution was interrupted")?;
                audit::tool_finished(tool.name(), &result).await;
                if name == "record_routine_step" && result.is_ok() {
                    asked_for_step = false;
                }
                if let Some(lookup) = lookup {
                    let observation = lookup.observe(result);
                    if let Some(page) = lookup::opened_page(&observation) {
                        opened_page = Some(page);
                    }
                    let saved = can_plan
                        .then(|| lookup.verified_snapshot(&observation))
                        .flatten();
                    checkpoints::finish_read(
                        &app.db,
                        &app.vault,
                        task,
                        &action,
                        &observation.to_string(),
                        saved
                            .as_ref()
                            .map(|(key, evidence)| (key.as_str(), evidence.as_str())),
                    )
                    .await?;
                    lookups.insert(lookup);
                } else if finish(&ctx, &action, result, false).await? {
                    return Ok(());
                }
                if name == "set_goal" {
                    goal = sqlx::query_scalar("SELECT goal FROM tasks WHERE id=?")
                        .bind(&task.id)
                        .fetch_one(&app.db)
                        .await?;
                }
            }
        }
    }
}

async fn finish_from_context(
    app: &App,
    task: &Task,
    feedback: Option<&Feedback>,
    model: &dyn ModelProvider,
    input: &Value,
    stop_reason: Option<&str>,
) -> Result<()> {
    if !store::current(&app.db, task).await? {
        return store::settle(&app.db, task, "idle", None).await;
    }
    let text = match lookup::reply(model, input, stop_reason).await {
        Ok(text) => text,
        Err(error) => {
            if let Some(limit) = limits::Limit::from_error(&error) {
                let mut context = input.clone();
                context["tool_stop_reason"] = json!(stop_reason);
                match limit
                    .finish(app, task, feedback, model, &context, error)
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(error)
                        if provider_failure(&error).is_none() || input["task_plan"].is_null() =>
                    {
                        return Err(error);
                    }
                    Err(_) => {}
                }
            }
            task_plan::partial_reply(input, stop_reason)
                .unwrap_or_else(|| lookup::UNVERIFIED_REPLY.into())
        }
    };
    let _delivery = match feedback {
        Some(feedback) => Some(feedback.delivery.lock().await),
        None => None,
    };
    if !store::current(&app.db, task).await? {
        return store::settle(&app.db, task, "idle", None).await;
    }
    if task_plan::unfinished(input) && stop_reason.is_some() {
        store::settle(&app.db, task, "blocked", Some(&text)).await
    } else if stop_reason.is_none() {
        drop(_delivery);
        super::routine_reply::send(app, task, &text, &[], feedback).await
    } else {
        store::settle_reply(&app.db, task, &text, &[]).await
    }
}

async fn bind_shared_delegation(db: &sqlx::SqlitePool, task: &Task, args: &Value) -> Result<bool> {
    let Some(helper) = args["agent"].as_str().filter(|_| task.agent.is_none()) else {
        return Ok(false);
    };
    if super::string(args, "brief", 4000).is_err() {
        return Ok(false);
    }
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let updated = sqlx::query(
        "UPDATE tasks SET agent=?,state='queued',updated=?
        WHERE id=? AND user=? AND revision=? AND chief_chat=1 AND agent IS NULL AND state='running'
        AND EXISTS(SELECT 1 FROM agents a JOIN grants g ON g.agent=a.id
            WHERE a.id=? AND g.member=tasks.user AND a.owner<>tasks.user)",
    )
    .bind(helper)
    .bind(now())
    .bind(&task.id)
    .bind(&task.user)
    .bind(task.revision)
    .bind(helper)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 1 {
        sqlx::query(
            "INSERT INTO pending(id,user,task,revision,kind,payload,state,expires)
            VALUES(?,?,?,?,'chief-delegate',?,'approved',?)",
        )
        .bind(id())
        .bind(&task.user)
        .bind(&task.id)
        .bind(task.revision)
        .bind(args.to_string())
        .bind(now() + 900)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(updated == 1)
}
