use super::{feedback::Feedback, turn::work_task};
use crate::{
    App, audit,
    channel::ChannelAdapter,
    store::{self, Task},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

pub async fn run(app: Arc<App>, channel: Arc<dyn ChannelAdapter>) {
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
                            audit::scope(trace.clone(), audit::record("session", "agent", "running", Value::Null)).await;
                            let acknowledgement = tokio::spawn({
                                let app = app.clone();
                                let task = task.clone();
                                audit::scope(trace.clone(), async move {
                                    if super::acknowledgement::send(&app, &task).await.is_err() {
                                        tracing::debug!(task=%task.id, "Mercury acknowledgement unavailable");
                                    }
                                })
                            });
                            let result = audit::scope(trace.clone(), async {
                                let feedback=Feedback::default();
                                let processing=work_task(&app,&task,Some(&feedback));
                                tokio::pin!(processing);
                                tokio::select! {
                                    biased;
                                    result=&mut processing=>result,
                                    ()=feedback.show(&app.db,&task,&*channel)=>processing.await,
                                }
                            }).await;
                            acknowledgement.abort();
                            let _ = acknowledgement.await;
                            if result.is_err() {
                                tracing::warn!(task=%task.id,"task paused");
                                let _=store::settle(&app.db,&task,"blocked",Some(
                                    "I couldn’t finish this turn. Your task is saved. Send a follow-up to try again."
                                )).await;
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

#[cfg(test)]
pub(crate) async fn work(app: &App, task: &Task) -> anyhow::Result<()> {
    let trace = start_audit(app, task).await;
    let result = audit::scope(trace.clone(), async {
        audit::record("session", "agent", "running", Value::Null).await;
        work_task(app, task, None).await
    })
    .await;
    finish_audit(trace, task, result.is_err()).await;
    result
}

async fn start_audit(app: &App, task: &Task) -> Option<audit::Trace> {
    match audit::Trace::start(&app.db, task).await {
        Ok(trace) => trace,
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
