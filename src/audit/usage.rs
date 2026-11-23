use super::{CURRENT, MODEL_SCOPE, VISIBLE_SESSIONS};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{FromRow, Sqlite, SqliteConnection, Transaction};
use std::{collections::BTreeMap, time::Instant};

/// Numbers supplied by the provider. Missing fields stay distinct from valid zeroes.
#[derive(Default, Serialize)]
struct Usage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cost_usd: Option<f64>,
}

#[derive(Serialize)]
pub(crate) struct ModelCall {
    provider: String,
    model: String,
    usage: Usage,
}

impl ModelCall {
    pub(crate) fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: safe_id(Some(provider), b" ._()-")
                .unwrap_or("unknown")
                .to_owned(),
            model: safe_id(Some(model), b"./_:-~")
                .unwrap_or("unknown")
                .to_owned(),
            usage: Usage::default(),
        }
    }

    fn detail(&self, duration_ms: u64) -> Value {
        json!({
            "provider":self.provider,"model":self.model,"usage":self.usage,
            "duration_ms":duration_ms,
        })
    }

    pub(crate) fn has_tokens(&self) -> bool {
        self.usage.input_tokens.is_some()
            || self.usage.output_tokens.is_some()
            || self.usage.total_tokens.is_some()
    }

    pub(crate) fn model(&mut self, id: &Value) {
        if let Some(model) = safe_id(id.as_str(), b"./_:-~") {
            self.model = model.to_owned();
        }
    }

    pub(crate) fn openrouter(&mut self, response: &Value) {
        if let Some(provider) = safe_id(response["provider"].as_str(), b" ._()-") {
            self.provider = provider.to_owned();
        }
        self.model(&response["model"]);
        let usage = &response["usage"];
        self.usage.merge_report(Usage {
            input_tokens: tokens(&usage["prompt_tokens"])
                .or_else(|| tokens(&usage["input_tokens"])),
            output_tokens: tokens(&usage["completion_tokens"])
                .or_else(|| tokens(&usage["output_tokens"])),
            total_tokens: tokens(&usage["total_tokens"]),
            cost_usd: usage["cost"]
                .as_f64()
                .filter(|cost| cost.is_finite() && (0.0..=1_000_000.0).contains(cost)),
        });
    }

    pub(crate) fn codex(&mut self, total: &Value) {
        self.usage.merge_report(Usage {
            input_tokens: tokens(&total["inputTokens"]),
            output_tokens: tokens(&total["outputTokens"]),
            total_tokens: tokens(&total["totalTokens"]),
            cost_usd: None,
        });
    }
}

impl Usage {
    fn merge_report(&mut self, report: Self) {
        self.input_tokens = report.input_tokens.or(self.input_tokens);
        self.output_tokens = report.output_tokens.or(self.output_tokens);
        self.total_tokens = report.total_tokens.or(self.total_tokens);
        self.cost_usd = report.cost_usd.or(self.cost_usd);
    }
}

fn tokens(value: &Value) -> Option<i64> {
    // Also bounds the aggregation, including responses from third-party providers.
    value
        .as_i64()
        .filter(|count| (0..=1_000_000_000).contains(count))
}

fn safe_id<'a>(value: Option<&'a str>, punctuation: &[u8]) -> Option<&'a str> {
    let value = value?;
    let lower = value.to_ascii_lowercase();
    (!value.is_empty()
        && value == value.trim()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || punctuation.contains(&byte))
        && value.bytes().any(|byte| byte.is_ascii_alphabetic())
        && !lower.starts_with("sk-")
        && ![
            "sk-or-",
            "sk-proj-",
            "ghp_",
            "github_pat_",
            "xoxb-",
            "bearer ",
        ]
        .iter()
        .any(|secret| lower.contains(secret)))
    .then_some(value)
}

pub(crate) async fn provider_call<T>(
    provider: &'static str,
    model: &str,
    run: impl AsyncFnOnce(&mut ModelCall) -> Result<T>,
) -> Result<T> {
    let mut call = ModelCall::new(provider, model);
    let Ok(Some(trace)) = CURRENT.try_with(Clone::clone) else {
        return run(&mut call).await;
    };
    let name = MODEL_SCOPE
        .try_with(|name| *name)
        .unwrap_or("provider_call");
    let entry = trace
        .push("model_call", name, "started", &json!(call))
        .await;
    let start = Instant::now();
    let output = run(&mut call).await;
    let detail = call.detail(start.elapsed().as_millis() as u64);
    let status = if output.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let stored: Result<()> = match entry {
        Ok(Some(id)) => {
            sqlx::query("UPDATE audit_entries SET status=?,detail=? WHERE id=? AND session=?")
                .bind(status)
                .bind(detail.to_string())
                .bind(id)
                .bind(trace.session)
                .execute(&trace.db)
                .await
                .map(|_| ())
                .map_err(Into::into)
        }
        Ok(None) => Ok(()),
        Err(_) => trace
            .push("model_call", name, status, &detail)
            .await
            .map(|_| ()),
    };
    if stored.is_err() {
        tracing::warn!("unable to record model call audit entry");
    }
    output
}

pub(crate) async fn ingress_model_call(
    tx: &mut Transaction<'_, Sqlite>,
    task: &str,
    name: &str,
    usage: &ModelCall,
    duration_ms: u64,
) -> Result<()> {
    let at = crate::security::now();
    sqlx::query(
        "INSERT INTO audit_entries(session,attempt,at,kind,name,status,detail)
         SELECT s.id,0,?,'model_call',?,'completed',?
         FROM tasks t JOIN audit_sessions s ON s.id=t.audit_session_id
         WHERE t.id=? AND s.started_at>?",
    )
    .bind(at)
    .bind(name)
    .bind(usage.detail(duration_ms).to_string())
    .bind(task)
    .bind(at - super::RETENTION)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Default, FromRow, Serialize)]
struct Totals {
    calls: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cost_usd: Option<f64>,
    token_reported_calls: i64,
    cost_reported_calls: i64,
}

impl Totals {
    fn add(&mut self, row: &Self) {
        self.calls += row.calls;
        self.input_tokens = add_optional(self.input_tokens, row.input_tokens);
        self.output_tokens = add_optional(self.output_tokens, row.output_tokens);
        self.total_tokens = add_optional(self.total_tokens, row.total_tokens);
        self.cost_usd = add_optional(self.cost_usd, row.cost_usd);
        self.token_reported_calls += row.token_reported_calls;
        self.cost_reported_calls += row.cost_reported_calls;
    }
}

fn add_optional<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    }
}

#[derive(FromRow, Serialize)]
struct ModelRow {
    provider: Option<String>,
    model: Option<String>,
    #[sqlx(flatten)]
    #[serde(flatten)]
    totals: Totals,
}

pub(super) async fn summary(
    connection: &mut SqliteConnection,
    user: &str,
    session: Option<i64>,
    cutoff: i64,
) -> Result<Value> {
    let sql = format!(
        "WITH visible AS (SELECT s.id {VISIBLE_SESSIONS} AND (? IS NULL OR s.id=?)),
         reported AS (
            SELECT
              json_extract(e.detail,'$.provider') AS provider,
              json_extract(e.detail,'$.model') AS model,
              json_extract(e.detail,'$.usage.input_tokens') AS input_tokens,
              json_extract(e.detail,'$.usage.output_tokens') AS output_tokens,
              json_extract(e.detail,'$.usage.total_tokens') AS total_tokens,
              json_extract(e.detail,'$.usage.cost_usd') AS cost_usd
            FROM audit_entries e JOIN visible s ON s.id=e.session
            WHERE e.kind='model_call' OR (e.kind='model' AND e.status IN ('completed','failed')
                AND json_extract(e.detail,'$.usage_tracking') IS NULL)
         )
         SELECT provider,model,COUNT(*) AS calls,
           SUM(input_tokens) AS input_tokens,SUM(output_tokens) AS output_tokens,
           SUM(total_tokens) AS total_tokens,SUM(cost_usd) AS cost_usd,
           SUM(input_tokens IS NOT NULL OR output_tokens IS NOT NULL OR total_tokens IS NOT NULL)
             AS token_reported_calls,
           COUNT(cost_usd) AS cost_reported_calls
         FROM reported GROUP BY provider,model ORDER BY provider,model",
    );
    let rows = sqlx::query_as::<_, ModelRow>(&sql)
        .bind(user)
        .bind(cutoff)
        .bind(session)
        .bind(session)
        .fetch_all(connection)
        .await?;
    let mut total = Totals::default();
    let mut providers = BTreeMap::<String, Totals>::new();
    let mut models = Vec::new();
    let mut legacy_entries = 0;
    for row in rows {
        if let Some(provider) = &row.provider {
            total.add(&row.totals);
            providers
                .entry(provider.clone())
                .or_default()
                .add(&row.totals);
            models.push(row);
        } else {
            legacy_entries += row.totals.calls;
        }
    }
    #[derive(Serialize)]
    struct ProviderRow {
        provider: String,
        #[serde(flatten)]
        totals: Totals,
    }
    let providers: Vec<_> = providers
        .into_iter()
        .map(|(provider, totals)| ProviderRow { provider, totals })
        .collect();
    Ok(json!({"total":total,"providers":providers,"models":models,"legacy_entries":legacy_entries}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{audit, security::now, store};
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn records_each_call_once_with_actual_attribution_and_unknown_measurements() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "1").await.unwrap();
        sqlx::query(
            "UPDATE users SET provider='openrouter',model='fixture/requested' WHERE id='1'",
        )
        .execute(&db)
        .await
        .unwrap();
        store::ingest(&db, "1", "test", 1, "1", "What can these fixtures do?")
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        let trace = audit::Trace::start(&db, &task).await.unwrap().unwrap();
        let id = trace.session;
        // An old runtime entry stays available but has no attributable usage.
        trace
            .push(
                "model",
                "agent_turn",
                "completed",
                &json!({"duration_ms":80}),
            )
            .await
            .unwrap();
        audit::scope(Some(trace), async {
            audit::model("mercury_acknowledgement", async {
                provider_call("openrouter", "inception/mercury-3", async |call| {
                    call.openrouter(&json!({
                        "provider":"Fixture Compute","model":"fixture/actual",
                        "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5,"cost":0}
                    }));
                    Ok(())
                })
                .await?;
                provider_call::<()>("openrouter", "inception/mercury-3", async |call| {
                    call.openrouter(
                        &json!({"provider":"Fixture Compute","model":"fixture/actual"}),
                    );
                    anyhow::bail!("failed fixture sk-or-provider-body")
                })
                .await
            })
            .await
            .unwrap_err();
            provider_call("codex", "fixture/codex", async |call| {
                call.codex(&json!({"inputTokens":0,"outputTokens":0,"totalTokens":0}));
                Ok(())
            })
            .await
            .unwrap();
        })
        .await;
        let page = audit::sessions(&db, "1", Some(id)).await.unwrap();
        assert_eq!(page["sessions"], json!([]));
        let usage = &page["usage"];
        assert_eq!(usage["legacy_entries"], 1);
        assert_eq!(
            usage["total"],
            json!({
                "calls":3,"input_tokens":2,"output_tokens":3,"total_tokens":5,
                "token_reported_calls":2,"cost_reported_calls":1,"cost_usd":0.0
            })
        );
        assert_eq!(usage["models"][0]["provider"], "Fixture Compute");
        assert_eq!(usage["models"][0]["model"], "fixture/actual");
        assert_eq!(usage["providers"][0]["calls"], 2);
        assert_eq!(usage["providers"][1]["cost_usd"], Value::Null);
        let run = audit::session(&db, "1", id, 0).await.unwrap();
        assert_eq!(run["usage"], *usage);
        assert!(!run.to_string().contains("sk-or-provider-body"));
        assert!(
            run["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["kind"] == "model_call"
                    && entry["name"] == "mercury_acknowledgement"
                    && entry["status"] == "failed")
        );
        sqlx::query("UPDATE audit_sessions SET started_at=? WHERE id=?")
            .bind(now() - super::super::RETENTION - 1)
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        let expired = audit::sessions(&db, "1", None).await.unwrap();
        assert_eq!(expired["usage"]["total"]["calls"], 0);
        assert_eq!(expired["usage"]["total"]["cost_usd"], Value::Null);
    }
}
