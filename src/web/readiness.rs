use crate::{
    App,
    workers::{WorkerHealth, valid_release},
};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde_json::json;
use std::{sync::Arc, time::Duration};

pub(super) async fn ready(State(app): State<Arc<App>>) -> impl IntoResponse {
    let (database, (browser, sandbox)) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&app.db)
        ),
        app.workers.check(),
    );
    let release = option_env!("TINYBOT_RELEASE_SHA").filter(|value| valid_release(value));
    let database = matches!(database, Ok(Ok(1)));
    let ready =
        database && matches_release(&browser, release) && matches_release(&sandbox, release);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(
            json!({"ready":ready,"release":release,"database":database,"browser":browser,"sandbox":sandbox}),
        ),
    )
}

fn matches_release(worker: &Option<WorkerHealth>, expected: Option<&str>) -> bool {
    worker.as_ref().is_some_and(|value| {
        value.ok && expected.is_none_or(|expected| value.release.as_deref() == Some(expected))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labelled_rollouts_require_healthy_workers_at_the_same_revision() {
        let revision = "a".repeat(40);
        let reported = Some(WorkerHealth {
            ok: true,
            release: Some(revision.clone()),
        });
        assert!(matches_release(&reported, Some(&revision)));
        assert!(matches_release(&reported, None));
        assert!(!matches_release(&reported, Some(&"b".repeat(40))));
        assert!(!matches_release(&None, None));
        assert!(!matches_release(
            &Some(WorkerHealth {
                ok: false,
                release: None
            }),
            None
        ));
    }
}
