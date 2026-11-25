use super::{feedback::Feedback, turn};
use crate::{
    App, audit,
    channel::ChannelAdapter,
    store::{self, Task},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

async fn work_task(app: &App, task: &Task, feedback: Option<&Feedback>) -> anyhow::Result<()> {
    if let Some(preview) = crate::schedules::preview_context(&app.db, task).await? {
        return super::preview::run(app, task, preview).await;
    }
    turn::work_task(app, task, feedback).await
}

pub async fn run(app: Arc<App>, channel: Arc<dyn ChannelAdapter>) {
    tokio::join!(run_tasks(app.clone(), channel), run_acknowledgements(app));
}

async fn run_tasks(app: Arc<App>, channel: Arc<dyn ChannelAdapter>) {
    let mut jobs = tokio::task::JoinSet::new();
    let mut cleanup = tokio::time::interval(Duration::from_secs(12));
    loop {
        tokio::select! {
            _=cleanup.tick()=>{
                if store::expire(&app.db).await.is_err() || app.forms.prune(&app.db).await.is_err() {
                    tracing::warn!("expiry cleanup failed");
                }
            }
            Some(done)=jobs.join_next(), if !jobs.is_empty()=> {
                if done.is_err() {tracing::error!("task worker crashed");}
            }
            _=tokio::time::sleep(Duration::from_millis(550)), if jobs.len()<6=>{
                match store::claim(&app.db).await {
                    Ok(Some(task))=>{
                        let app=app.clone();
                        let channel=channel.clone();
                        jobs.spawn(async move {
                            let trace = start_audit(&app, &task).await;
                            let group = store::groups::is_group_task(&app.db, &task.id).await.unwrap_or(true);
                            let result = audit::scope(trace.clone(), async {
                                if group {
                                    return work_task(&app, &task, None).await;
                                }
                                let feedback=Feedback::default();
                                let processing=work_task(&app,&task,Some(&feedback));
                                tokio::pin!(processing);
                                tokio::select! {
                                    biased;
                                    result=&mut processing=>result,
                                    ()=feedback.show(&app.db,&task,&*channel)=>processing.await,
                                }
                            }).await;
                            if let Err(error) = &result {
                                record_failure(&app, &task, trace.clone(), group, error).await;
                            }
                            finish_audit(trace, &task, result.is_err()).await;
                        });
                    }
                    Ok(None)=>{},
                    Err(_)=>tracing::warn!("unable to claim task")
                }
            }
        }
    }
}

async fn run_acknowledgements(app: Arc<App>) {
    let mut jobs = tokio::task::JoinSet::new();
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            Some(done) = jobs.join_next(), if !jobs.is_empty() => {
                if done.is_err() {
                    tracing::debug!("progress generation worker stopped");
                }
            }
            _ = poll.tick(), if jobs.len() < 2 => {
                match store::acknowledgements::claim(&app.db).await {
                    Ok(Some(ready)) => {
                        let app = app.clone();
                        jobs.spawn(async move {
                            let trace = audit::Trace::for_progress(
                                &app.db, &ready.task, &ready.user, ready.revision
                            ).await.ok().flatten();
                            let confirmed_steps = match (&trace, ready.queued) {
                                (Some(trace), false) => trace.confirmed_progress_steps().await.unwrap_or(0),
                                _ => 0,
                            };
                            if !ready.queued && confirmed_steps == 0
                                && store::acknowledgements::retry_for_evidence(&app.db, &ready).await.unwrap_or(false)
                            {
                                return;
                            }
                            let reply = audit::scope(trace, super::acknowledgement::generate(
                                &app, &ready.user, &ready.request, ready.queued, confirmed_steps,
                            )).await.ok().flatten();
                            if store::acknowledgements::finish(&app.db, &ready, reply.as_deref()).await.is_err() {
                                tracing::debug!("progress update could not enter delivery");
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(_) => tracing::debug!("progress work could not be claimed"),
                }
            }
        }
    }
}

async fn record_failure(
    app: &App,
    task: &Task,
    trace: Option<audit::Trace>,
    group: bool,
    error: &anyhow::Error,
) {
    let failure = super::failure::classify(error);
    tracing::warn!(task=%task.id,category=failure.category,"task paused");
    audit::scope(
        trace,
        audit::record(
            "session",
            "failure",
            "blocked",
            serde_json::json!({"category":failure.category}),
        ),
    )
    .await;
    let explanation = if group {
        String::from("I couldn’t finish this group task. Reply here to continue.")
    } else {
        super::failure::explain(&app.db, task, &failure)
            .await
            .unwrap_or_else(|_| failure.message.into())
    };
    let recorded = if group {
        store::settle(&app.db, task, "blocked", Some(&explanation)).await
    } else {
        store::task_failures::record(&app.db, task, &explanation, failure.retryable).await
    };
    if recorded.is_err() {
        tracing::warn!(task=%task.id,"could not save task failure");
    }
}

#[cfg(test)]
pub(crate) async fn work(app: &App, task: &Task) -> anyhow::Result<()> {
    let trace = start_audit(app, task).await;
    let result = audit::scope(trace.clone(), work_task(app, task, None)).await;
    if let Err(error) = &result {
        let group = store::groups::is_group_task(&app.db, &task.id).await?;
        record_failure(app, task, trace.clone(), group, error).await;
    }
    finish_audit(trace, task, result.is_err()).await;
    result
}

async fn start_audit(app: &App, task: &Task) -> Option<audit::Trace> {
    match audit::Trace::start(&app.db, task).await {
        Ok(trace) => {
            audit::scope(
                trace.clone(),
                audit::record(
                    "session",
                    "agent",
                    "running",
                    json!({"revision":task.revision}),
                ),
            )
            .await;
            trace
        }
        Err(_) => {
            tracing::warn!("unable to start agent audit session");
            None
        }
    }
}

async fn finish_audit(trace: Option<audit::Trace>, task: &Task, failed: bool) {
    if let Some(trace) = trace
        && trace.finish(task, failed).await.is_err()
    {
        tracing::warn!("unable to finish agent audit session");
    }
}
