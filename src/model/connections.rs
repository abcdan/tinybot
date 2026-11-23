use super::Models;
use crate::{security::clip, siwc::Profile};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sqlx::{Sqlite, Transaction};

#[derive(Serialize)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct ModelConnections {
    pub openrouter_key: bool,
    pub codex_model: Option<String>,
}

#[derive(Debug)]
pub enum OpenRouterSettingsError {
    MissingKey,
    InvalidKey,
    InvalidModel,
    Unavailable,
    Changed,
}

impl std::fmt::Display for OpenRouterSettingsError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::MissingKey => "Save an OpenRouter API key first.",
            Self::InvalidKey => "OpenRouter rejected this key. Use a standard OpenRouter API key.",
            Self::InvalidModel => "Choose a tool-capable model available with this OpenRouter key.",
            Self::Unavailable => {
                "OpenRouter model settings could not be verified. Try again shortly."
            }
            Self::Changed => "The OpenRouter key changed. Reload the model list and choose again.",
        })
    }
}

impl std::error::Error for OpenRouterSettingsError {}

pub(super) struct UserKey {
    pub(super) plaintext: String,
    encrypted: String,
}

impl Models {
    pub async fn connections(&self, user: &str) -> Result<ModelConnections> {
        let (openrouter_key, codex_model): (bool, Option<String>) = sqlx::query_as(
            "SELECT openrouter_key IS NOT NULL OR (provider IS 'openrouter' AND credential IS NOT NULL),
                    CASE WHEN codex_credential IS NOT NULL THEN codex_model
                         WHEN provider IS 'codex' AND credential IS NOT NULL THEN model END
             FROM users WHERE id=?",
        )
        .bind(user)
        .fetch_one(&self.db)
        .await?;
        Ok(ModelConnections {
            openrouter_key,
            codex_model,
        })
    }

    pub(super) async fn user_openrouter_key(&self, user: &str) -> Result<Option<UserKey>> {
        // The Chief and Mercury can load the same legacy key concurrently.
        // The CAS loser reads the winner's new ciphertext rather than aborting the task.
        for _ in 0..2 {
            let (stored, legacy): (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT openrouter_key, CASE WHEN provider='openrouter' THEN credential END
                 FROM users WHERE id=?",
            )
            .bind(user)
            .fetch_one(&self.db)
            .await?;
            let binding = format!("openrouter:{user}");
            if legacy.is_none()
                && let Some(encrypted) = &stored
                && let Ok(plain) = self.vault.open(&binding, encrypted)
            {
                return Ok(Some(UserKey {
                    plaintext: String::from_utf8(plain)?,
                    encrypted: encrypted.clone(),
                }));
            }
            // The migration transfers old task ciphertext without key material.
            // Read that binding only for a legacy OpenRouter key and atomically re-seal it.
            let Some(old) = legacy.as_ref().or(stored.as_ref()) else {
                return Ok(None);
            };
            let plaintext = String::from_utf8(self.vault.open(&format!("model:{user}"), old)?)?;
            ensure!(
                valid_key(&plaintext),
                "invalid legacy OpenRouter credential"
            );
            let encrypted = self.vault.seal(&binding, plaintext.as_bytes())?;
            let updated = sqlx::query(
                "UPDATE users SET openrouter_key=?, credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END
                 WHERE id=? AND openrouter_key IS ? AND (CASE WHEN provider='openrouter' THEN credential END) IS ?",
            )
            .bind(&encrypted)
            .bind(user)
            .bind(&stored)
            .bind(&legacy)
            .execute(&self.db)
            .await?
            .rows_affected();
            if updated == 1 {
                return Ok(Some(UserKey {
                    plaintext,
                    encrypted,
                }));
            }
        }
        Err(OpenRouterSettingsError::Changed.into())
    }

    pub async fn save_openrouter_key(&self, user: &str, key: &str) -> Result<Vec<OpenRouterModel>> {
        ensure!(valid_key(key), OpenRouterSettingsError::InvalidKey);
        let (_, catalog) = tokio::try_join!(verify_key(key), openrouter_catalog(key))?;
        self.save_verified_openrouter_key(user, key).await?;
        Ok(catalog)
    }

    async fn save_verified_openrouter_key(&self, user: &str, key: &str) -> Result<()> {
        let encrypted = self
            .vault
            .seal(&format!("openrouter:{user}"), key.as_bytes())?;
        sqlx::query(
            "UPDATE users SET openrouter_key=?, credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END WHERE id=?",
        )
        .bind(encrypted)
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn openrouter_models(&self, user: &str) -> Result<Vec<OpenRouterModel>> {
        let key = self
            .user_openrouter_key(user)
            .await?
            .context(OpenRouterSettingsError::MissingKey)?;
        openrouter_catalog(&key.plaintext).await
    }

    pub async fn select_openrouter_model(&self, user: &str, model: &str) -> Result<()> {
        ensure!(valid_model_id(model), OpenRouterSettingsError::InvalidModel);
        let key = self
            .user_openrouter_key(user)
            .await?
            .context(OpenRouterSettingsError::MissingKey)?;
        let catalog = openrouter_catalog(&key.plaintext).await?;
        ensure!(
            catalog.iter().any(|entry| entry.id == model),
            OpenRouterSettingsError::InvalidModel
        );
        self.activate_openrouter(user, model, &key).await
    }

    async fn activate_openrouter(&self, user: &str, model: &str, key: &UserKey) -> Result<()> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE users SET provider='openrouter', model=?, credential=NULL
             WHERE id=? AND openrouter_key=?",
        )
        .bind(model)
        .bind(user)
        .bind(&key.encrypted)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(updated == 1, OpenRouterSettingsError::Changed);
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn connect_codex(&self, user: &str, model: &str, profile: &Profile) -> Result<()> {
        ensure!(
            !model.is_empty()
                && model.len() <= 150
                && model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)),
            "invalid Codex model"
        );
        let encrypted = self
            .vault
            .seal(&format!("model:{user}"), &serde_json::to_vec(profile)?)?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE users SET provider='codex', model=?, codex_model=?, codex_credential=?, credential=NULL WHERE id=?",
        )
        .bind(model)
        .bind(model)
        .bind(encrypted)
        .bind(user)
        .execute(&mut *tx)
        .await?;
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn select_codex(&self, user: &str) -> Result<()> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE users SET provider='codex', model=codex_model, credential=NULL
             WHERE id=? AND codex_model IS NOT NULL AND codex_credential IS NOT NULL",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        ensure!(updated == 1, "pair ChatGPT for Codex first");
        queue_setup_tasks(&mut tx, user).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn disconnect_model(&self, user: &str) -> Result<()> {
        sqlx::query("UPDATE users SET provider=NULL,model=NULL WHERE id=?")
            .bind(user)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn disconnect_openrouter(&self, user: &str) -> Result<()> {
        sqlx::query(
            "UPDATE users SET openrouter_key=NULL,
             model=CASE WHEN provider='openrouter' THEN NULL ELSE model END,
             credential=CASE WHEN provider='openrouter' THEN NULL ELSE credential END,
             provider=CASE WHEN provider='openrouter' THEN NULL ELSE provider END
             WHERE id=?",
        )
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn disconnect_codex(&self, user: &str) -> Result<()> {
        sqlx::query(
            "UPDATE users SET codex_model=NULL,codex_credential=NULL,
             model=CASE WHEN provider='codex' THEN NULL ELSE model END,
             credential=CASE WHEN provider='codex' THEN NULL ELSE credential END,
             provider=CASE WHEN provider='codex' THEN NULL ELSE provider END
             WHERE id=?",
        )
        .bind(user)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

async fn queue_setup_tasks(tx: &mut Transaction<'_, Sqlite>, user: &str) -> Result<()> {
    sqlx::query("UPDATE tasks SET state='queued' WHERE user=? AND state='setup'")
        .bind(user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn valid_key(key: &str) -> bool {
    key.strip_prefix("sk-or-").is_some_and(|key| {
        !key.is_empty()
            && key.len() <= 8192
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    })
}

fn valid_model_id(model: &str) -> bool {
    model.len() <= 150
        && !model.starts_with("typesafe/jev-1")
        && model != "~typesafe/jev-latest"
        && model.split_once('/').is_some_and(|(author, name)| {
            [author, name].into_iter().all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.:~@+".contains(&b))
            })
        })
}

async fn openrouter_get(path: &str, key: &str, limit: usize) -> Result<serde_json::Value> {
    let url = crate::net::https(path)?;
    let response = crate::net::request(reqwest::Method::GET, &url)
        .await
        .map_err(|_| OpenRouterSettingsError::Unavailable)?
        .bearer_auth(key)
        .send()
        .await
        .map_err(|_| OpenRouterSettingsError::Unavailable)?;
    match response.status() {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            return Err(OpenRouterSettingsError::InvalidKey.into());
        }
        status if !status.is_success() => {
            return Err(OpenRouterSettingsError::Unavailable.into());
        }
        _ => {}
    }
    let body = crate::net::body(response, limit)
        .await
        .map_err(|_| OpenRouterSettingsError::Unavailable)?;
    serde_json::from_slice(&body).map_err(|_| OpenRouterSettingsError::Unavailable.into())
}

async fn verify_key(key: &str) -> Result<()> {
    let response = openrouter_get("https://openrouter.ai/api/v1/key", key, 16 * 1024).await?;
    let info = response["data"]
        .as_object()
        .context(OpenRouterSettingsError::Unavailable)?;
    ensure!(
        !["is_provisioning_key", "is_management_key"]
            .iter()
            .any(|field| info
                .get(*field)
                .is_some_and(|value| value.as_bool() == Some(true))),
        OpenRouterSettingsError::InvalidKey
    );
    Ok(())
}

async fn openrouter_catalog(key: &str) -> Result<Vec<OpenRouterModel>> {
    let body = openrouter_get(
        "https://openrouter.ai/api/v1/models/user?sort=most-popular",
        key,
        4 * 1024 * 1024,
    )
    .await?;
    parse_catalog(&body)
}

fn parse_catalog(body: &serde_json::Value) -> Result<Vec<OpenRouterModel>> {
    let models = body["data"]
        .as_array()
        .context(OpenRouterSettingsError::Unavailable)?;
    let mut seen = std::collections::HashSet::new();
    Ok(models
        .iter()
        .filter_map(|model| {
            let id = model["id"].as_str().filter(|id| valid_model_id(id))?;
            if !supports(&model["supported_parameters"], "tools")
                || model["endpoints"].is_array()
                    && !supports(&model["endpoints"], "/api/v1/chat/completions")
                || model["architecture"]["input_modalities"].is_array()
                    && !supports(&model["architecture"]["input_modalities"], "text")
                || model["architecture"]["output_modalities"].is_array()
                    && !supports(&model["architecture"]["output_modalities"], "text")
                || !seen.insert(id)
            {
                return None;
            }
            let name = model["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
                .unwrap_or(id);
            Some(OpenRouterModel {
                id: id.to_owned(),
                name: clip(name, 150).to_owned(),
            })
        })
        .collect())
}

fn supports(values: &serde_json::Value, expected: &str) -> bool {
    values
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value == expected))
}

#[cfg(test)]
mod tests {
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
        sqlx::query("UPDATE users SET provider='openrouter',model='provider/tool',credential=? WHERE id='3'")
            .bind(old_task).execute(&db).await.unwrap();
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

    #[test]
    fn model_catalog_only_offers_chat_models_that_can_use_task_tools() {
        let catalog = parse_catalog(&json!({"data":[
            {"id":"a/tool-model:free","name":"Tool Model","supported_parameters":["tools"],"endpoints":["/api/v1/chat/completions"]},
            {"id":"a/tool-model:free","name":"Repeated ID","supported_parameters":["tools"]},
            {"id":"a/text-model","name":"Text Model","supported_parameters":["temperature"]},
            {"id":"a/responses-only","supported_parameters":["tools"],"endpoints":["/api/v1/responses"]},
            {"id":"a/image","supported_parameters":["tools"],"architecture":{"output_modalities":["image"]}},
            {"id":"typesafe/jev-1.14","supported_parameters":["tools"]},
            {"id":"not/a/slug","supported_parameters":["tools"]}
        ]})).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "a/tool-model:free");
        assert_eq!(catalog[0].name, "Tool Model");
    }
}
