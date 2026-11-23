mod codex;
mod openrouter;

use self::{codex::Codex, openrouter::OpenRouter};
use crate::{
    security::{Vault, now},
    siwc::Profile,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type TextDelta<'a> = dyn FnMut(&str) + Send + 'a;

pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

fn native_tool_proposal(tools: &[ToolDefinition], name: &str, args: Value) -> Result<String> {
    ensure!(
        tools.iter().any(|tool| tool.name == name),
        "unavailable native tool"
    );
    ensure!(args.is_object(), "invalid native tool arguments");
    let args = serde_json::to_string(&args)?;
    ensure!(args.len() < 30_000, "oversized native tool arguments");
    Ok(format!(
        r#"{{"kind":"tool","name":{},"args":{args}}}"#,
        serde_json::to_string(name)?
    ))
}

fn native_reply(response: String) -> Result<String> {
    if let Ok(value) = parse_json_reply::<Value>(&response) {
        ensure!(
            value["kind"] != "tool",
            "expected a native provider tool call"
        );
    }
    Ok(response)
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, system: &str, input: &str) -> Result<String>;

    async fn complete_streaming(
        &self,
        system: &str,
        input: &str,
        _delta: &mut TextDelta<'_>,
    ) -> Result<String> {
        self.complete(system, input).await
    }

    async fn complete_turn(
        &self,
        system: &str,
        input: &str,
        _tools: &[ToolDefinition],
        delta: Option<&mut TextDelta<'_>>,
    ) -> Result<String> {
        match delta {
            Some(delta) => self.complete_streaming(system, input, delta).await,
            None => self.complete(system, input).await,
        }
    }
}

pub struct Models {
    db: SqlitePool,
    vault: Vault,
    codex_binary: String,
    review_model: Option<String>,
    review_key: Option<String>,
    refresh: Mutex<()>,
    #[cfg(test)]
    fixture: Option<Arc<dyn ModelProvider>>,
}

impl Models {
    pub fn new(
        db: SqlitePool,
        vault: Vault,
        codex_binary: String,
        review_model: Option<String>,
        review_key: Option<String>,
    ) -> Self {
        Self {
            db,
            vault,
            codex_binary,
            review_model,
            review_key,
            refresh: Mutex::new(()),
            #[cfg(test)]
            fixture: None,
        }
    }

    #[cfg(test)]
    pub fn with_fixture(mut self, model: Arc<dyn ModelProvider>) -> Self {
        self.fixture = Some(model);
        self
    }

    async fn configured_openrouter_key(&self, user: &str) -> Result<Option<String>> {
        let (auxiliary, task): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT openrouter_key,
            CASE WHEN provider='openrouter' THEN credential END FROM users WHERE id=?",
        )
        .bind(user)
        .fetch_one(&self.db)
        .await?;
        if let Some(key) = auxiliary {
            return Ok(Some(String::from_utf8(
                self.vault.open(&format!("openrouter:{user}"), &key)?,
            )?));
        }
        if let Some(key) = task {
            return Ok(Some(String::from_utf8(
                self.vault.open(&format!("model:{user}"), &key)?,
            )?));
        }
        Ok(self.review_key.clone())
    }

    async fn openrouter_key(&self, user: &str) -> Result<String> {
        self.configured_openrouter_key(user).await?.context(
            "Add an OpenRouter key under Settings & forms for the configured decision model.",
        )
    }

    pub async fn mercury(&self, user: &str) -> Result<Option<Arc<dyn ModelProvider>>> {
        let Some(key) = self.configured_openrouter_key(user).await? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(OpenRouter {
            key,
            model: "inception/mercury-3".into(),
        })))
    }

    pub async fn forms(&self, user: &str) -> Result<Arc<dyn ModelProvider>> {
        #[cfg(test)]
        if let Some(model) = &self.fixture {
            return Ok(model.clone());
        }
        match self.mercury(user).await? {
            Some(mercury) => Ok(mercury),
            None => self.provider(user, false).await,
        }
    }

    pub async fn decisions(&self, user: &str, input: &Value) -> Result<Option<Value>> {
        let Some(model) = self
            .review_model
            .as_deref()
            .filter(|id| id.starts_with("typesafe/jev-1") || *id == "~typesafe/jev-latest")
        else {
            return Ok(None);
        };
        let endpoint = crate::net::https("https://openrouter.ai/api/alpha/decisions")?;
        let response=crate::net::request(reqwest::Method::POST,&endpoint).await?
            .bearer_auth(self.openrouter_key(user).await?).json(&json!({
                "model":model,"state":input,
                "questions":{"review":{"type":"choice","instructions":"Choose the safest appropriate handling for this proposed operation.",
                    "criteria":{"allow":"An ordinary, authorized read or draft that can proceed.",
                        "confirm":"An external modification, submission, disclosure, or ambiguous operation that needs human review.",
                        "deny":"Clearly unsafe, disallowed, or requests credentials outside an authorized setup."}}}
            })).send().await?.error_for_status()?;
        let response: Value = serde_json::from_slice(&crate::net::body(response, 64_000).await?)?;
        ensure!(
            response["answers"]["review"]["type"] == "choice",
            "invalid Jev answer type"
        );
        let answer = response["answers"]["review"]["choice"]
            .as_str()
            .context("missing Jev decision")?;
        let reason = match answer {
            "allow" => "The decision model classified this as an ordinary action.",
            "confirm" => "The decision model requested human review.",
            "deny" => "The decision model flagged this action as unsafe.",
            _ => anyhow::bail!("unknown Jev decision"),
        };
        Ok(Some(json!({"verdict":answer,"reason":reason})))
    }

    pub async fn provider(&self, user: &str, review: bool) -> Result<Arc<dyn ModelProvider>> {
        #[cfg(test)]
        if let Some(model) = &self.fixture {
            return Ok(model.clone());
        }
        if review && let Some(model) = &self.review_model {
            return Ok(Arc::new(OpenRouter {
                key: self.openrouter_key(user).await?,
                model: model.clone(),
            }));
        }
        let _refresh = self.refresh.lock().await;
        let (kind, model, credential): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT provider,model,credential FROM users WHERE id=?")
                .bind(user)
                .fetch_one(&self.db)
                .await?;
        let kind = kind.context("Choose a provider in the Mini App first.")?;
        let model = model.context("Choose a model in the Mini App first.")?;
        let binding = format!("model:{user}");
        let credential = credential.context("model credential missing")?;
        let secret = self.vault.open(&binding, &credential)?;
        if kind == "openrouter" {
            return Ok(Arc::new(OpenRouter {
                key: String::from_utf8(secret)?,
                model,
            }));
        }
        ensure!(kind == "codex", "unknown model provider");
        let mut profile: Profile = serde_json::from_slice(&secret)?;
        if profile.expires <= now() + 90 {
            profile
                .refresh()
                .await
                .context("Reconnect ChatGPT in the Mini App.")?;
            let enc = self.vault.seal(&binding, &serde_json::to_vec(&profile)?)?;
            sqlx::query(
                "UPDATE users SET credential=? WHERE id=? AND provider='codex' AND credential=?",
            )
            .bind(enc)
            .bind(user)
            .bind(credential)
            .execute(&self.db)
            .await?;
        }
        Ok(Arc::new(Codex {
            access: profile.access_token,
            model,
            binary: self.codex_binary.clone(),
        }))
    }
}

pub async fn json_reply<T: DeserializeOwned>(
    model: &dyn ModelProvider,
    instructions: &str,
    input: &Value,
) -> Result<T> {
    let response = model.complete(instructions, &input.to_string()).await?;
    parse_json_reply(&response)
}

pub fn parse_json_reply<T: DeserializeOwned>(response: &str) -> Result<T> {
    let text = response.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|fenced| fenced.strip_suffix("```"))
        .unwrap_or(text)
        .trim();
    Ok(serde_json::from_str(text).context("model returned an invalid structured response")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn codex_can_generate_forms_without_an_openrouter_key() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        crate::store::seed(&db, "1").await.unwrap();
        let vault = Vault::new(&"f1".repeat(32)).unwrap();
        let profile = json!({
            "client_id":"test","access_token":"dummy","refresh_token":"dummy",
            "id_token":"dummy","subject":"test","expires":now()+600
        });
        let credential = vault
            .seal("model:1", profile.to_string().as_bytes())
            .unwrap();
        sqlx::query(
            "UPDATE users SET provider='codex',model='test-model',credential=? WHERE id='1'",
        )
        .bind(credential)
        .execute(&db)
        .await
        .unwrap();
        let models = Models::new(db, vault, "codex".into(), None, None);
        assert!(models.forms("1").await.is_ok());
    }
}
