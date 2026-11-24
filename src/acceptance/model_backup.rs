use super::{app_with_model, fixture, message};
use crate::{
    model::{ModelProvider, Models, ProviderFailure},
    runtime, store,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Provider {
    calls: AtomicUsize,
    result: &'static str,
}

impl Provider {
    fn new(result: &'static str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            result,
        })
    }
}

#[async_trait]
impl ModelProvider for Provider {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match self.result {
            "temporary" => Err(ProviderFailure::Temporary.into()),
            "permanent" => Err(anyhow!("invalid model credential")),
            reply => Ok(json!({"kind":"reply","text":reply}).to_string()),
        }
    }
}

#[tokio::test]
async fn transient_failure_switches_once_per_revision_and_survives_recovery() {
    let db = fixture().await;
    let primary = Provider::new("temporary");
    let backup = Provider::new("Backup reply");
    let mut app = app_with_model(&db, primary.clone());
    app.models = Models::new(db.clone(), app.vault.clone(), "codex".into(), None, None)
        .with_fixture(primary.clone())
        .with_backup_fixture(backup.clone());
    sqlx::query(
        "UPDATE users SET codex_model='test-backup',codex_credential='fixture' WHERE id='1'",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(
        app.models
            .save_backup("2", "codex", "test-backup")
            .await
            .is_err()
    );
    app.models
        .save_backup("1", "codex", "test-backup")
        .await
        .unwrap();
    message(&db, 1, "1", "Continue this task").await;
    let task = store::claim(&db).await.unwrap().unwrap();
    runtime::work(&app, &task).await.unwrap();
    assert_eq!(primary.calls.load(Ordering::Relaxed), 1);
    assert_eq!(backup.calls.load(Ordering::Relaxed), 1);
    let bodies: Vec<String> =
        sqlx::query_scalar("SELECT body FROM outbox WHERE task=? ORDER BY id")
            .bind(&task.id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("ChatGPT for Codex · test-backup"));
    assert_eq!(bodies[1], "Backup reply");

    sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    store::recover(&db).await.unwrap();
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert_eq!(resumed.revision, task.revision);
    runtime::work(&app, &resumed).await.unwrap();
    assert_eq!(primary.calls.load(Ordering::Relaxed), 1);
    assert_eq!(backup.calls.load(Ordering::Relaxed), 2);
    let notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE task=? AND body LIKE '%try your backup%'",
    )
    .bind(&task.id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(notices, 1);

    app.models.clear_backup("1").await.unwrap();
    sqlx::query("UPDATE tasks SET state='running' WHERE id=?")
        .bind(&task.id)
        .execute(&db)
        .await
        .unwrap();
    store::recover(&db).await.unwrap();
    let resumed = store::claim(&db).await.unwrap().unwrap();
    assert!(runtime::work(&app, &resumed).await.is_err());
    assert_eq!(primary.calls.load(Ordering::Relaxed), 2);
    assert_eq!(backup.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn backup_skips_permanent_errors_and_revisions_that_attempted_tools() {
    for (failure, has_tool) in [("permanent", false), ("temporary", true)] {
        let db = fixture().await;
        let primary = Provider::new(failure);
        let backup = Provider::new("Unexpected backup");
        let mut app = app_with_model(&db, primary.clone());
        app.models = Models::new(db.clone(), app.vault.clone(), "codex".into(), None, None)
            .with_fixture(primary.clone())
            .with_backup_fixture(backup.clone());
        sqlx::query(
            "UPDATE users SET codex_model='test-backup',codex_credential='fixture' WHERE id='1'",
        )
        .execute(&db)
        .await
        .unwrap();
        app.models
            .save_backup("1", "codex", "test-backup")
            .await
            .unwrap();
        message(&db, 1, "1", "Continue this task").await;
        let task = store::claim(&db).await.unwrap().unwrap();
        if has_tool {
            assert!(
                store::tool_budget::record(&db, &task, "chief", "browse", &json!({}))
                    .await
                    .unwrap()
            );
        }
        assert!(runtime::work(&app, &task).await.is_err());
        assert_eq!(backup.calls.load(Ordering::Relaxed), 0);
        assert!(
            store::model_backup::selected(&db, &task)
                .await
                .unwrap()
                .is_none()
        );
    }
}
