use super::OpenRouterSettingsError;
use crate::security::clip;
use anyhow::{Context, Result, ensure};
use serde::Serialize;

#[derive(Serialize)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
}

pub(super) fn valid_model_id(model: &str) -> bool {
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

pub(super) async fn verify_key(key: &str) -> Result<()> {
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

pub(super) async fn openrouter_catalog(key: &str) -> Result<Vec<OpenRouterModel>> {
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
    use serde_json::json;

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
