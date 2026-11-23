use super::{Intent, Outcome, Risk, Tool, ToolContext, string};
use crate::{
    audit,
    model::{ServerTool, ServerToolInput},
    net, proxy, schedules,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

pub(super) fn tools() -> Vec<Box<dyn Tool>> {
    ServerTool::ALL
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn Tool>)
        .collect()
}

pub(super) async fn available(ctx: &ToolContext<'_>) -> Result<Vec<&'static str>> {
    if !ctx.owner || ctx.parent_task.is_some() {
        return Ok(Vec::new());
    }
    let web = hosted_web_allowed();
    Ok(ctx
        .app
        .models
        .enabled_server_tools(&ctx.task.user)
        .await?
        .into_iter()
        .filter(|tool| !tool.hosted_web() || web)
        .map(|tool| tool.name())
        .collect())
}

fn hosted_web_allowed() -> bool {
    proxy::browser_proxy_status()
        .is_ok_and(|status| allows_hosted_web(status.mode, status.deployment_configured))
}

fn allows_hosted_web(mode: &str, deployment_configured: bool) -> bool {
    mode == "direct" || (mode == "deployment" && !deployment_configured)
}

#[async_trait]
impl Tool for ServerTool {
    fn name(&self) -> &'static str {
        match self {
            Self::WebSearch => "openrouter_web_search",
            Self::WebFetch => "openrouter_web_fetch",
            Self::Datetime => "openrouter_datetime",
            Self::SearchModels => "openrouter_search_models",
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::WebSearch => {
                "Search public information with OpenRouter's hosted web search. Supply only a public query without customer records, private task details, or credentials."
            }
            Self::WebFetch => {
                "Read one public HTTPS page with OpenRouter's hosted fetch. Supply only a public URL without private links or access credentials."
            }
            Self::Datetime => {
                "Check the current date and time via OpenRouter for the saved time zone or an explicitly requested IANA time zone."
            }
            Self::SearchModels => {
                "Search OpenRouter's model catalog by name or capability. This tool only finds model information and cannot switch models."
            }
        }
    }

    fn input_schema(&self) -> Value {
        let (key, max, required) = match self {
            Self::WebSearch | Self::SearchModels => ("query", 240, true),
            Self::WebFetch => ("url", 2048, true),
            Self::Datetime => ("timezone", 100, false),
        };
        json!({"type":"object","properties":{(key):{"type":"string","maxLength":max}},
            "required":if required {vec![key]} else {vec![]},"additionalProperties":false})
    }

    async fn inspect(&self, ctx: &ToolContext<'_>, args: &Value) -> Result<Intent> {
        ensure!(
            available(ctx).await?.contains(&self.name()),
            "OpenRouter server tool is unavailable for this task"
        );
        match self {
            Self::WebSearch | Self::SearchModels => {
                public_query(args)?;
            }
            Self::WebFetch => {
                public_url(args)?;
            }
            Self::Datetime if args.get("timezone").is_some() => {
                timezone_arg(args)?;
            }
            Self::Datetime => {}
        }
        Ok(Intent {
            risk: Risk::PublicRead,
            destination: "OpenRouter".into(),
            detail: self.name().into(),
        })
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> Result<Outcome> {
        self.inspect(ctx, &args).await?;
        let input = match self {
            Self::WebSearch => ServerToolInput::WebSearch(public_query(&args)?),
            Self::SearchModels => ServerToolInput::SearchModels(public_query(&args)?),
            Self::WebFetch => {
                let url = public_url(&args)?;
                net::resolve(
                    url.host_str()
                        .context("missing fetch host")?
                        .trim_matches(['[', ']']),
                    443,
                )
                .await?;
                ServerToolInput::WebFetch(url)
            }
            Self::Datetime => ServerToolInput::Datetime(if args.get("timezone").is_some() {
                timezone_arg(&args)?
            } else {
                schedules::timezone(&ctx.app.db, &ctx.task.user).await?
            }),
        };
        ensure!(
            ctx.still_allows(self.name()).await?,
            "task or server tool authorization changed"
        );
        ensure!(
            !self.hosted_web() || hosted_web_allowed(),
            "use the local browser tools with the configured proxy"
        );
        let result = audit::model(
            self.name(),
            ctx.app.models.run_server_tool(&ctx.task.user, input),
        )
        .await?;
        Ok(Outcome::Continue(result))
    }
}

fn timezone_arg(args: &Value) -> Result<chrono_tz::Tz> {
    string(args, "timezone", 100)?
        .parse()
        .context("invalid IANA time zone")
}

fn public_query(args: &Value) -> Result<&str> {
    let query = string(args, "query", 240)?.trim();
    ensure!(
        !query.chars().any(char::is_control)
            && !contains_credential(query)
            && query
                .split_whitespace()
                .all(|word| Url::parse(word).map_or(true, |url| {
                    !matches!(url.scheme(), "http" | "https") || !has_url_credential(&url)
                })),
        "use only a public search query"
    );
    Ok(query)
}

fn public_url(args: &Value) -> Result<Url> {
    let mut url = net::website(string(args, "url", 2048)?)?;
    url.set_fragment(None);
    ensure!(
        !has_url_credential(&url),
        "use a public URL without access credentials"
    );
    ensure!(
        match url.host() {
            Some(url::Host::Ipv4(ip)) => net::public(ip.into()),
            Some(url::Host::Ipv6(ip)) => net::public(ip.into()),
            _ => true,
        },
        "destination is not public"
    );
    Ok(url)
}

fn has_url_credential(url: &Url) -> bool {
    !url.username().is_empty()
        || url.password().is_some()
        || contains_credential(url.path())
        || url.query_pairs().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            [
                "token",
                "password",
                "credential",
                "secret",
                "signature",
                "authorization",
            ]
            .iter()
            .any(|tag| key.contains(tag))
                || matches!(
                    key.as_str(),
                    "sig" | "key" | "apikey" | "api_key" | "session" | "sessionid" | "auth"
                )
                || contains_credential(&value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_web_preserves_saved_and_deployed_browser_proxy_preference() {
        assert!(allows_hosted_web("direct", true));
        assert!(allows_hosted_web("deployment", false));
        assert!(!allows_hosted_web("deployment", true));
        assert!(!allows_hosted_web("proxy", false));
    }

    #[test]
    fn hosted_web_accepts_only_bounded_public_inputs() {
        assert_eq!(
            public_query(&json!({"query":" current Dutch elections "})).unwrap(),
            "current Dutch elections"
        );
        assert!(public_query(&json!({"query":"password=fixture"})).is_err());
        assert!(public_query(&json!({"query":"visit https://demo:fixture@example.org"})).is_err());
        assert!(public_query(&json!({"query":"example.org\nsecond line"})).is_err());
        assert_eq!(
            public_url(&json!({"url":"example.org/docs?q=public#section"}))
                .unwrap()
                .as_str(),
            "https://example.org/docs?q=public"
        );
        for url in [
            "https://demo:fixture@example.org",
            "https://example.org/?token=fixture",
            "https://127.0.0.1",
            "https://[::1]/",
        ] {
            assert!(public_url(&json!({"url":url})).is_err(), "{url}");
        }
    }
}

fn contains_credential(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "sk-or-",
        "sk-proj-",
        "bearer ",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "-----begin ",
        "password=",
        "password:",
        "api_key=",
        "api key:",
        "access_token=",
        "authorization:",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}
