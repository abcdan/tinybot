use super::*;
use crate::{security::Vault, store};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

async fn fixture() -> (SqlitePool, Models, [OpenRouterModel; 1]) {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!().run(&db).await.unwrap();
    for user in ["1", "2"] {
        store::seed(&db, user).await.unwrap();
    }
    let models = Models::new(
        db.clone(),
        Vault::new(&"bf".repeat(32)).unwrap(),
        "codex".into(),
        None,
        None,
    );
    let catalog = [OpenRouterModel {
        id: "fixture/model".into(),
        name: "Fixture Model".into(),
    }];
    (db, models, catalog)
}

#[tokio::test]
async fn onboarding_persists_profile_and_encrypted_key_once_and_queues_only_the_owners_tasks() {
    let (db, models, catalog) = fixture().await;
    for user in ["1", "2"] {
        sqlx::query(
            "INSERT INTO tasks(id,user,title,state,updated) VALUES(?,?,'Starter','setup',0)",
        )
        .bind(user)
        .bind(user)
        .execute(&db)
        .await
        .unwrap();
    }
    assert!(models.onboarding_profile("1").await.unwrap().required);
    let failed = models
        .complete_verified_onboarding(
            "1",
            "Dana",
            "Chief One",
            "sk-or-fixture",
            "fixture/not-in-catalog",
            &catalog,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        failed.downcast_ref::<OpenRouterSettingsError>(),
        Some(OpenRouterSettingsError::InvalidModel)
    ));
    assert!(models.onboarding_profile("1").await.unwrap().required);

    let (first, second) = tokio::join!(
        models.complete_verified_onboarding(
            "1",
            "Dana",
            "Chief One",
            "sk-or-fixture-one",
            "fixture/model",
            &catalog
        ),
        models.complete_verified_onboarding(
            "1",
            "Rae",
            "Chief Two",
            "sk-or-fixture-two",
            "fixture/model",
            &catalog
        )
    );
    let (name, chief, key, rejected) = match (first, second) {
        (Ok(()), Err(error)) => ("Dana", "Chief One", "sk-or-fixture-one", error),
        (Err(error), Ok(())) => ("Rae", "Chief Two", "sk-or-fixture-two", error),
        results => panic!("expected one successful onboarding: {results:?}"),
    };
    assert!(matches!(
        rejected.downcast_ref::<OnboardingError>(),
        Some(OnboardingError::AlreadyConfigured)
    ));
    let profile = models.onboarding_profile("1").await.unwrap();
    assert!(!profile.required);
    assert_eq!(profile.display_name.as_deref(), Some(name));
    assert_eq!(profile.chief_name.as_deref(), Some(chief));
    let (provider, model, sealed): (String, String, String) =
        sqlx::query_as("SELECT provider,model,openrouter_key FROM users WHERE id='1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "fixture/model");
    assert!(!sealed.contains(key));
    assert_eq!(
        models.vault.open("openrouter:1", &sealed).unwrap(),
        key.as_bytes()
    );
    assert!(models.vault.open("openrouter:2", &sealed).is_err());
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM tasks ORDER BY id")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(states, ["queued", "setup"]);
    assert!(models.onboarding_profile("2").await.unwrap().required);

    models.disconnect_openrouter("1").await.unwrap();
    assert!(!models.onboarding_profile("1").await.unwrap().required);
}

#[tokio::test]
async fn legacy_provider_or_saved_key_skips_onboarding_and_labels_reject_controls() {
    let (db, models, _) = fixture().await;
    sqlx::query("UPDATE users SET provider='codex',model='fixture-codex' WHERE id='1'")
        .execute(&db)
        .await
        .unwrap();
    models
        .save_verified_openrouter_key("2", "sk-or-fixture-legacy")
        .await
        .unwrap();
    for user in ["1", "2"] {
        let profile = models.onboarding_profile(user).await.unwrap();
        assert!(!profile.required);
        assert!(profile.display_name.is_none());
        assert!(profile.chief_name.is_none());
        let error = models
            .onboarding_models(user, "sk-or-must-not-be-sent")
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error.downcast_ref::<OnboardingError>(),
            Some(OnboardingError::AlreadyConfigured)
        ));
    }
    assert_eq!(display_label("  Zoë  "), Some("Zoë"));
    assert!(display_label(&"🎈".repeat(64)).is_some());
    assert_eq!(display_label(&"🎈".repeat(65)), None);
    assert_eq!(display_label("  "), None);
    assert_eq!(display_label("\nChief"), None);
}
