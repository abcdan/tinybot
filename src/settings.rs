use crate::{
    App,
    forms::{Field, FormSpec},
    model::{
        IMAGE_MODELS, ModelPreferences, ModelSelection, display_label, resolve_image_model,
        write_image_model,
    },
};
use anyhow::{Context, Result, ensure};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqliteConnection;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Key {
    DisplayName,
    ChiefName,
    TimeZone,
    WebSearch,
    WebFetch,
    Datetime,
    SearchModels,
    TaskModel,
    BackupModel,
    ImageModel,
}

impl Key {
    pub(crate) const ALL: [Self; 10] = [
        Self::DisplayName,
        Self::ChiefName,
        Self::TimeZone,
        Self::WebSearch,
        Self::WebFetch,
        Self::Datetime,
        Self::SearchModels,
        Self::TaskModel,
        Self::BackupModel,
        Self::ImageModel,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DisplayName => "display_name",
            Self::ChiefName => "chief_name",
            Self::TimeZone => "time_zone",
            Self::WebSearch => "web_search",
            Self::WebFetch => "web_fetch",
            Self::Datetime => "datetime",
            Self::SearchModels => "search_models",
            Self::TaskModel => "task_model",
            Self::BackupModel => "backup_model",
            Self::ImageModel => "image_model",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::DisplayName => "Your display name",
            Self::ChiefName => "Chief name",
            Self::TimeZone => "Time zone",
            Self::WebSearch => "OpenRouter web search",
            Self::WebFetch => "OpenRouter page fetch",
            Self::Datetime => "OpenRouter date and time",
            Self::SearchModels => "OpenRouter model search",
            Self::TaskModel => "Task model",
            Self::BackupModel => "Backup model",
            Self::ImageModel => "Image generation and editing model",
        }
    }

    pub(crate) fn hint(self) -> Option<&'static str> {
        match self {
            Self::DisplayName | Self::ChiefName => {
                Some("Up to 64 characters. Leave blank to use the default.")
            }
            Self::TimeZone => Some("Use an IANA time zone, such as Europe/Amsterdam."),
            Self::TaskModel => {
                Some("Use codex or openrouter:provider/model for a model you've connected.")
            }
            Self::BackupModel => Some(
                "Use codex, openrouter:provider/model, or off. Choose a different provider from your task model.",
            ),
            _ => None,
        }
    }

    pub(crate) fn field(self) -> Field {
        let options: Vec<String> = if self.toggle() {
            vec!["on".into(), "off".into()]
        } else if self == Self::ImageModel {
            IMAGE_MODELS.iter().map(|model| model.id.into()).collect()
        } else {
            Vec::new()
        };
        Field {
            name: self.as_str().into(),
            label: self.label().into(),
            kind: if options.is_empty() { "text" } else { "select" }.into(),
            required: !matches!(self, Self::DisplayName | Self::ChiefName),
            options,
            private: false,
            hint: self.hint().map(str::to_owned),
        }
    }

    fn toggle(self) -> bool {
        matches!(
            self,
            Self::WebSearch | Self::WebFetch | Self::Datetime | Self::SearchModels
        )
    }

    fn model(self) -> bool {
        matches!(self, Self::TaskModel | Self::BackupModel)
    }

    pub(crate) fn history_eligible(self) -> bool {
        !self.model()
    }

    fn normalize(self, value: &str) -> Result<String> {
        let trimmed = value.trim();
        match self {
            Self::DisplayName | Self::ChiefName if value.is_empty() => Ok(String::new()),
            Self::DisplayName | Self::ChiefName => display_label(value)
                .map(str::to_owned)
                .context("Enter a name of up to 64 characters."),
            Self::TimeZone => Ok(trimmed
                .parse::<Tz>()
                .context("Enter an IANA time zone, such as Europe/Amsterdam.")?
                .name()
                .into()),
            Self::TaskModel | Self::BackupModel => {
                ensure!(
                    !trimmed.is_empty()
                        && trimmed.len() <= 161
                        && !value.chars().any(char::is_control),
                    "Enter codex or openrouter:provider/model for a connected model."
                );
                Ok(trimmed.to_owned())
            }
            Self::ImageModel => IMAGE_MODELS
                .iter()
                .find(|model| model.id == trimmed)
                .map(|model| model.id.into())
                .context("Choose an available image model."),
            _ => match trimmed.to_ascii_lowercase().as_str() {
                "on" | "true" | "enabled" => Ok("on".into()),
                "off" | "false" | "disabled" => Ok("off".into()),
                _ => anyhow::bail!("Choose on or off."),
            },
        }
    }
}

pub(crate) fn form(keys: &[Key]) -> Result<FormSpec> {
    let spec = FormSpec {
        fields: keys.iter().map(|key| key.field()).collect(),
        mini_app: true,
    };
    spec.validate()?;
    Ok(spec)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    values: BTreeMap<Key, String>,
    models: ModelPreferences,
}

impl Snapshot {
    pub(crate) fn value(&self, key: Key) -> &str {
        self.values.get(&key).map_or("", String::as_str)
    }
}

pub(crate) async fn snapshot(db: &mut SqliteConnection, user: &str) -> Result<Snapshot> {
    let (display_name, chief_name, time_zone, image_model): (Option<String>, Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT display_name,chief_name,time_zone,image_model FROM users WHERE id=? AND suspended_at IS NULL")
            .bind(user)
            .fetch_one(&mut *db)
            .await?;
    let time_zone = time_zone
        .or_else(|| std::env::var("TINYBOT_TIMEZONE").ok())
        .unwrap_or_else(|| "UTC".into());
    let models = ModelPreferences::read(&mut *db, user).await?;
    let mut values = BTreeMap::from([
        (Key::DisplayName, display_name.unwrap_or_default()),
        (Key::ChiefName, chief_name.unwrap_or_default()),
        (Key::TimeZone, time_zone),
        (Key::TaskModel, models.task_setting()),
        (Key::BackupModel, models.backup_setting()),
        (
            Key::ImageModel,
            resolve_image_model(image_model.as_deref()).into(),
        ),
    ]);
    let disabled: Vec<String> =
        sqlx::query_scalar("SELECT tool FROM openrouter_disabled_tools WHERE user=?")
            .bind(user)
            .fetch_all(db)
            .await?;
    for key in Key::ALL.into_iter().filter(|key| key.toggle()) {
        let value = if disabled.iter().any(|tool| tool == key.as_str()) {
            "off"
        } else {
            "on"
        };
        values.insert(key, value.into());
    }
    Ok(Snapshot { values, models })
}

pub(crate) async fn read(app: &App, user: &str) -> Result<Value> {
    let mut tx = app.db.begin().await?;
    let current = snapshot(&mut tx, user).await?;
    let connectors: Vec<(String, String)> =
        sqlx::query_as("SELECT name,auth FROM connectors WHERE owner=? ORDER BY name LIMIT 40")
            .bind(user)
            .fetch_all(&mut *tx)
            .await?;
    let helpers: Vec<String> =
        sqlx::query_scalar("SELECT name FROM agents WHERE owner=? ORDER BY name LIMIT 40")
            .bind(user)
            .fetch_all(&mut *tx)
            .await?;
    let (mail, smtp, inbox): (i64, bool, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(max(can_send),0), coalesce(max(
           EXISTS(SELECT 1 FROM mail_folders WHERE account=mail_accounts.id)
        ),0) FROM mail_accounts WHERE owner=?",
    )
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let connectors: Vec<Value> = connectors
        .into_iter()
        .map(|(name, auth)| json!({"name":name,"connected":auth!="pending"}))
        .collect();
    Ok(json!({
        "settings":current.values,
        "models":current.models.public(),
        "image_models":IMAGE_MODELS,
        "connections":connectors,
        "owned_helpers":helpers,
        "email":{"account_count":mail,"sending_enabled":smtp,"inbox_watched":inbox}
    }))
}

#[derive(Debug)]
pub(crate) struct Stale;

impl std::fmt::Display for Stale {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Your settings changed before approval. Open a fresh form to review them.")
    }
}

impl std::error::Error for Stale {}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    before: BTreeMap<Key, String>,
    after: BTreeMap<Key, String>,
    model_state: Option<ModelPreferences>,
}

#[derive(Serialize)]
pub(crate) struct Preview {
    pub key: Key,
    pub label: &'static str,
    pub before: String,
    pub after: String,
}

impl Change {
    pub(crate) fn personal_values(&self) -> impl Iterator<Item = (Key, &str, &str)> {
        self.after.iter().filter_map(|(&key, after)| {
            key.history_eligible().then(|| {
                (
                    key,
                    self.before.get(&key).map_or("", String::as_str),
                    after.as_str(),
                )
            })
        })
    }

    pub(crate) fn preview(&self) -> Vec<Preview> {
        self.after
            .iter()
            .map(|(&key, after)| {
                let before = self.before.get(&key).map_or("", String::as_str);
                let describe = |value: &str| {
                    if key.model() {
                        if let Some(models) = &self.model_state {
                            return models.describe(value);
                        }
                    }
                    if key == Key::ImageModel
                        && let Some(model) = IMAGE_MODELS.iter().find(|model| model.id == value)
                    {
                        return model.name.into();
                    }
                    match value {
                        "" => "Default".into(),
                        "on" if key.toggle() => "On".into(),
                        "off" if key.toggle() => "Off".into(),
                        _ => value.into(),
                    }
                };
                Preview {
                    key,
                    label: key.label(),
                    before: describe(before),
                    after: describe(after),
                }
            })
            .collect()
    }

    fn verify(&self, current: &Snapshot) -> Result<()> {
        ensure!(
            self.before
                .iter()
                .all(|(&key, value)| current.value(key) == value)
                && self
                    .model_state
                    .as_ref()
                    .is_none_or(|expected| expected == &current.models),
            Stale
        );
        Ok(())
    }
}

pub(crate) async fn prepare(
    app: &App,
    user: &str,
    fields: &BTreeMap<Key, String>,
    baseline: &Snapshot,
) -> Result<Change> {
    let change = prepare_values(fields, baseline)?;
    let selections = check_models(&change, baseline)?;
    if !selections.is_empty() {
        app.models
            .validate_settings_models(user, &selections)
            .await?;
    }
    let mut tx = app.db.begin().await?;
    change.verify(&snapshot(&mut tx, user).await?)?;
    tx.commit().await?;
    Ok(change)
}

pub(crate) fn prepare_values(
    fields: &BTreeMap<Key, String>,
    baseline: &Snapshot,
) -> Result<Change> {
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    for (&key, raw) in fields {
        let new = key.normalize(raw)?;
        let old = baseline.value(key);
        if new != old {
            before.insert(key, old.to_owned());
            after.insert(key, new);
        }
    }
    let includes_model = after.keys().any(|key| key.model());
    Ok(Change {
        before,
        after,
        model_state: includes_model.then(|| baseline.models.clone()),
    })
}

fn check_models<'a>(change: &'a Change, baseline: &'a Snapshot) -> Result<Vec<ModelSelection<'a>>> {
    let mut selected = Vec::new();
    if change.model_state.is_some() {
        let get = |key| {
            change
                .after
                .get(&key)
                .map_or(baseline.value(key), String::as_str)
        };
        let task = get(Key::TaskModel);
        let backup = get(Key::BackupModel);
        let current_provider = |value: &str| {
            if value == "codex" || value.starts_with("codex:") {
                "codex"
            } else if value.starts_with("openrouter:") {
                "openrouter"
            } else {
                ""
            }
        };
        ensure!(
            backup == "off"
                || (!current_provider(task).is_empty()
                    && current_provider(task) != current_provider(backup)),
            "Choose a backup model on a different provider, or set the backup to off."
        );
        for (&key, value) in &change.after {
            if key.model() && (key != Key::BackupModel || value != "off") {
                selected.push(ModelSelection::parse(value, &baseline.models)?);
            }
        }
    }
    Ok(selected)
}

pub(crate) async fn apply(db: &mut SqliteConnection, user: &str, change: &Change) -> Result<()> {
    let current = snapshot(&mut *db, user).await?;
    change.verify(&current)?;
    check_models(change, &current)?;
    for (key, _, value) in change.personal_values() {
        write_personal(db, user, key, value).await?;
    }
    current
        .models
        .apply(
            db,
            user,
            change.after.get(&Key::TaskModel).map(String::as_str),
            change.after.get(&Key::BackupModel).map(String::as_str),
        )
        .await
}

pub(crate) async fn update_personal(
    db: &sqlx::SqlitePool,
    user: &str,
    key: Key,
    input: &str,
) -> Result<()> {
    ensure!(key.history_eligible(), "unsupported personal setting");
    let value = key.normalize(input)?;
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let current = snapshot(&mut tx, user).await?;
    let before = current.value(key);
    write_personal(&mut tx, user, key, &value).await?;
    if before != value {
        crate::store::settings_history::record(
            &mut tx,
            user,
            None,
            "settings",
            [(key, before, value.as_str())],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn write_personal(
    db: &mut SqliteConnection,
    user: &str,
    key: Key,
    value: &str,
) -> Result<()> {
    let statement = match key {
        Key::DisplayName => "UPDATE users SET display_name=? WHERE id=?",
        Key::ChiefName => "UPDATE users SET chief_name=? WHERE id=?",
        Key::TimeZone => "UPDATE users SET time_zone=? WHERE id=?",
        Key::ImageModel => return write_image_model(db, user, value).await,
        Key::TaskModel | Key::BackupModel => anyhow::bail!("unsupported personal setting"),
        _ => {
            let statement = if value == "on" {
                "DELETE FROM openrouter_disabled_tools WHERE user=? AND tool=?"
            } else {
                "INSERT OR IGNORE INTO openrouter_disabled_tools(user,tool) VALUES(?,?)"
            };
            sqlx::query(statement)
                .bind(user)
                .bind(key.as_str())
                .execute(db)
                .await?;
            return Ok(());
        }
    };
    sqlx::query(statement)
        .bind((!value.is_empty()).then_some(value))
        .bind(user)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

    async fn account() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "7").await.unwrap();
        db
    }

    #[test]
    fn validates_labels_time_zones_and_flags() {
        assert_eq!(Key::ChiefName.normalize("   Pip   ").unwrap(), "Pip");
        assert!(Key::ChiefName.normalize("Pip\n").is_err());
        assert!(Key::ChiefName.normalize(&"🦦".repeat(65)).is_err());
        assert_eq!(
            Key::TimeZone.normalize(" Europe/Amsterdam ").unwrap(),
            "Europe/Amsterdam"
        );
        assert!(Key::TimeZone.normalize("GMT+1").is_err());
        assert_eq!(Key::WebSearch.normalize("true").unwrap(), "on");
        assert!(Key::WebSearch.normalize("sometimes").is_err());
        assert!(form(&[Key::ChiefName, Key::ChiefName]).is_err());
    }

    #[tokio::test]
    async fn changing_a_reviewed_value_prevents_the_entire_approval() {
        let db = account().await;
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let before = snapshot(&mut tx, "7").await.unwrap();
        sqlx::query("UPDATE users SET display_name='Changed separately' WHERE id='7'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let change = Change {
            before: BTreeMap::from([
                (Key::DisplayName, before.value(Key::DisplayName).into()),
                (Key::WebSearch, before.value(Key::WebSearch).into()),
            ]),
            after: BTreeMap::from([
                (Key::DisplayName, "Ada".into()),
                (Key::WebSearch, "off".into()),
            ]),
            model_state: None,
        };
        assert!(
            apply(&mut tx, "7", &change)
                .await
                .unwrap_err()
                .is::<Stale>()
        );
        let current = snapshot(&mut tx, "7").await.unwrap();
        assert_eq!(current.value(Key::DisplayName), "Changed separately");
        assert_eq!(current.value(Key::WebSearch), "on");
    }

    #[tokio::test]
    async fn models_require_a_distinct_backup_and_pin_the_connections() {
        let db = account().await;
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query(
            "UPDATE users SET provider='openrouter', model='fixture/active',
               openrouter_key='fixture-openrouter-key', codex_credential='fixture-codex-profile',
               codex_model='fixture-codex', backup_provider='codex', backup_model='fixture-codex'
             WHERE id='7'",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        let baseline = snapshot(&mut tx, "7").await.unwrap();
        let mut change = Change {
            before: BTreeMap::from([(Key::TaskModel, baseline.value(Key::TaskModel).into())]),
            after: BTreeMap::from([(Key::TaskModel, "codex".into())]),
            model_state: Some(baseline.models.clone()),
        };
        assert!(apply(&mut tx, "7", &change).await.is_err());
        change
            .before
            .insert(Key::BackupModel, baseline.value(Key::BackupModel).into());
        change.after.insert(Key::BackupModel, "off".into());
        let review = serde_json::to_string(&change.preview()).unwrap();
        assert!(review.contains("fixture-codex") && review.contains("Off"));
        let persisted = serde_json::to_string(&change).unwrap();
        assert!(
            !persisted.contains("fixture-openrouter-key")
                && !persisted.contains("fixture-codex-profile")
        );
        sqlx::query("UPDATE users SET codex_credential='rotated-fixture' WHERE id='7'")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            apply(&mut tx, "7", &change)
                .await
                .unwrap_err()
                .is::<Stale>()
        );
        sqlx::query("UPDATE users SET codex_credential='fixture-codex-profile' WHERE id='7'")
            .execute(&mut *tx)
            .await
            .unwrap();
        apply(&mut tx, "7", &change).await.unwrap();
        let saved = snapshot(&mut tx, "7").await.unwrap();
        assert_eq!(saved.value(Key::TaskModel), "codex");
        assert_eq!(saved.value(Key::BackupModel), "off");
    }
}
