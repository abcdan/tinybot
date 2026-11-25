use super::*;
use crate::{
    security::{Vault, now},
    store,
};
use serde_json::json;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

async fn database() -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap()
}

fn profile() -> Profile {
    serde_json::from_value(json!({
        "client_id":"fixture","access_token":"dummy","refresh_token":"dummy",
        "id_token":"dummy","subject":"fixture","expires":now()+600
    }))
    .unwrap()
}

#[tokio::test]
async fn ready_credentials_do_not_wait_on_codex_refresh() {
    use std::{pin::pin, time::Duration};
    use tokio::time::timeout;

    let db = database().await;
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["1", "2", "3"] {
        store::seed(&db, user).await.unwrap();
    }
    let vault = Vault::new(&"fa".repeat(32)).unwrap();
    let models = Models::new(db.clone(), vault.clone(), "codex".into(), None, None);
    let fresh = profile();
    models
        .connect_codex("1", "codex-fixture", &fresh)
        .await
        .unwrap();
    let mut expired = profile();
    expired.expires = now() - 60;
    models
        .connect_codex("3", "codex-fixture", &expired)
        .await
        .unwrap();
    let expired = models.codex_credential("3").await.unwrap();
    let key = vault.seal("openrouter:2", b"sk-or-fixture").unwrap();
    sqlx::query(
        "UPDATE users SET provider='openrouter',model='provider/tool',openrouter_key=? WHERE id='2'",
    )
    .bind(key)
    .execute(&db)
    .await
    .unwrap();

    let in_progress = models.refresh.lock().await;
    let mut waiting = pin!(models.codex_profile("3", &expired));
    assert!(timeout(Duration::ZERO, waiting.as_mut()).await.is_err());
    timeout(Duration::from_secs(2), async {
        assert!(models.provider("1", false).await.is_ok());
        assert!(models.provider("2", false).await.is_ok());
    })
    .await
    .unwrap();

    models
        .connect_codex("3", "codex-fixture", &fresh)
        .await
        .unwrap();
    drop(in_progress);
    assert_eq!(waiting.await.unwrap().expires, fresh.expires);
}

async fn active(db: &SqlitePool) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT provider,model FROM users WHERE id='1'")
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_preserves_task_and_auxiliary_keys_and_reseals_legacy_ones() {
    let db = database().await;
    let migrator = sqlx::migrate!();
    for migration in migrator.iter().filter(|migration| migration.version < 10) {
        sqlx::raw_sql(&migration.sql).execute(&db).await.unwrap();
    }
    let vault = Vault::new(&"fa".repeat(32)).unwrap();
    for user in ["1", "2", "3"] {
        store::seed(&db, user).await.unwrap();
    }
    let old_task = vault.seal("model:1", b"sk-or-fixture-task").unwrap();
    let old_auxiliary = vault.seal("openrouter:1", b"sk-or-fixture-aux").unwrap();
    sqlx::query("UPDATE users SET provider='openrouter',model='provider/tool',credential=?,openrouter_key=? WHERE id='1'")
        .bind(old_task).bind(old_auxiliary).execute(&db).await.unwrap();
    let old_codex = vault
        .seal("model:2", &serde_json::to_vec(&profile()).unwrap())
        .unwrap();
    let old_auxiliary = vault
        .seal("openrouter:2", b"sk-or-fixture-codex-aux")
        .unwrap();
    sqlx::query("UPDATE users SET provider='codex',model='fixture-codex',credential=?,openrouter_key=? WHERE id='2'")
        .bind(old_codex).bind(old_auxiliary).execute(&db).await.unwrap();
    let old_task = vault.seal("model:3", b"sk-or-fixture-task-only").unwrap();
    sqlx::query(
        "UPDATE users SET provider='openrouter',model='provider/tool',credential=? WHERE id='3'",
    )
    .bind(old_task)
    .execute(&db)
    .await
    .unwrap();
    let migration = migrator
        .iter()
        .find(|migration| migration.version == 10)
        .unwrap();
    sqlx::raw_sql(&migration.sql).execute(&db).await.unwrap();

    let models = Models::new(db.clone(), vault.clone(), "codex".into(), None, None);
    let (chief, mercury) = tokio::join!(
        models.user_openrouter_key("1"),
        models.user_openrouter_key("1"),
    );
    for snapshot in [chief.unwrap().unwrap(), mercury.unwrap().unwrap()] {
        assert!(snapshot.plaintext == "sk-or-fixture-task");
        assert!(vault.open("openrouter:1", &snapshot.encrypted).is_ok());
    }
    assert!(models.provider("1", false).await.is_ok());
    assert!(models.mercury("1").await.unwrap().is_some());
    assert!(models.provider("2", false).await.is_ok());
    assert_eq!(
        models
            .connections("2")
            .await
            .unwrap()
            .codex_model
            .as_deref(),
        Some("fixture-codex")
    );
    assert!(
        models
            .user_openrouter_key("2")
            .await
            .unwrap()
            .unwrap()
            .plaintext
            == "sk-or-fixture-codex-aux"
    );
    assert!(
        models
            .user_openrouter_key("3")
            .await
            .unwrap()
            .unwrap()
            .plaintext
            == "sk-or-fixture-task-only"
    );

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE credential IS NOT NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn switching_models_and_removing_keys_keeps_the_other_connection() {
    let db = database().await;
    sqlx::migrate!().run(&db).await.unwrap();
    store::seed(&db, "1").await.unwrap();
    let models = Models::new(
        db.clone(),
        Vault::new(&"fd".repeat(32)).unwrap(),
        "codex".into(),
        None,
        None,
    );

    models
        .connect_codex("1", "fixture-codex", &profile())
        .await
        .unwrap();
    models
        .save_verified_openrouter_key("1", "sk-or-fixture-a")
        .await
        .unwrap();
    assert_eq!(
        active(&db).await,
        (Some("codex".into()), Some("fixture-codex".into()))
    );
    assert!(models.mercury("1").await.unwrap().is_some());
    let first_key = models.user_openrouter_key("1").await.unwrap().unwrap();

    models
        .save_verified_openrouter_key("1", "sk-or-fixture-b")
        .await
        .unwrap();
    assert!(matches!(
        models
            .activate_openrouter("1", "vendor/model", &first_key)
            .await
            .unwrap_err()
            .downcast_ref::<OpenRouterSettingsError>(),
        Some(OpenRouterSettingsError::Changed)
    ));
    let current_key = models.user_openrouter_key("1").await.unwrap().unwrap();
    models
        .activate_openrouter("1", "vendor/model", &current_key)
        .await
        .unwrap();
    assert_eq!(
        active(&db).await,
        (Some("openrouter".into()), Some("vendor/model".into()))
    );
    assert!(models.provider("1", false).await.is_ok());

    models.disconnect_model("1").await.unwrap();
    assert_eq!(active(&db).await, (None, None));
    models.select_codex("1").await.unwrap();
    assert!(models.provider("1", false).await.is_ok());
    models.disconnect_openrouter("1").await.unwrap();
    assert_eq!(
        active(&db).await,
        (Some("codex".into()), Some("fixture-codex".into()))
    );
    assert!(models.mercury("1").await.unwrap().is_none());

    models
        .save_verified_openrouter_key("1", "sk-or-fixture-c")
        .await
        .unwrap();
    let current_key = models.user_openrouter_key("1").await.unwrap().unwrap();
    models
        .activate_openrouter("1", "vendor/model", &current_key)
        .await
        .unwrap();
    models.disconnect_codex("1").await.unwrap();
    assert_eq!(
        active(&db).await,
        (Some("openrouter".into()), Some("vendor/model".into()))
    );
    assert!(models.connections("1").await.unwrap().codex_model.is_none());
}
