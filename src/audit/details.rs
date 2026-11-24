use crate::{net, security::clip, store::REACTION_EMOJIS};
use serde_json::{Value, json};

pub(super) fn message_preview(text: &str) -> String {
    let lower = text.to_lowercase();
    if [
        "password",
        "passcode",
        "passwd",
        "bearer ",
        "api key",
        "api_key",
        "apikey",
        "secret",
        "token",
        "private key",
        "wachtwoord",
        "inlogcode",
        "verification code",
        "sk-or-",
        "sk-proj-",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "begin openssh",
    ]
    .iter()
    .any(|word| lower.contains(word))
        || text.trim().bytes().all(|byte| byte.is_ascii_digit())
    {
        return "[Sensitive text hidden]".into();
    }
    let mut preview = String::new();
    for word in text.split_whitespace() {
        if !preview.is_empty() {
            preview.push(' ');
        }
        if word.contains("//") {
            let url =
                word.trim_matches(['<', '>', '(', ')', '[', ']', '.', ',', '!', '?', '"', '\'']);
            preview.push_str(origin(url).as_deref().unwrap_or("[URL hidden]"));
        } else if (word.len() >= 28 && word.bytes().any(|byte| byte.is_ascii_digit()))
            || word.contains('@')
            || word.contains('=')
            || word.contains('?')
        {
            preview.push_str("[hidden]");
        } else {
            preview.push_str(word);
        }
        if preview.len() >= 260 {
            break;
        }
    }
    clip(&preview, 260).to_owned()
}

fn origin(target: &str) -> Option<String> {
    let url = net::website(target).ok()?;
    Some(clip(&url.origin().ascii_serialization(), 260).to_owned())
}

fn origin_field(value: &Value) -> Option<String> {
    value.as_str().and_then(origin)
}

fn len(value: &Value) -> usize {
    value.as_str().map_or(0, str::len)
}

fn count(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

pub(super) fn arguments(name: &str, args: &Value) -> Value {
    match name {
        "browse" | "request_form" => json!({"site":origin_field(&args["url"])}),
        "search_web" => json!({"query_bytes":len(&args["query"])}),
        "request_input" => json!({"field_count":count(&args["fields"]),
            "surface":match args["surface"].as_str() { Some("mini_app")=>"mini_app", _=>"auto" }
        }),
        "send_card" | "send_reply" => json!({"text_bytes":len(&args["text"]),
            "suggested_replies":count(&args["suggested_replies"])}),
        "react_to_message" => {
            json!({"emoji":args["emoji"].as_str().filter(|emoji|REACTION_EMOJIS.contains(emoji))})
        }
        "call_mcp" => json!({
            "argument_count":args["arguments"].as_object().map_or(0, |object|object.len()),
            "tool_name":args["tool"].as_str().filter(|value|value.len()<=128 &&
                value.bytes().all(|byte|byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
                .map(message_preview)
        }),
        _ => json!({"argument_count":args.as_object().map_or(0, |object|object.len())}),
    }
}

pub(super) fn result(name: &str, value: &Value) -> Value {
    match name {
        "browse" => json!({"site":origin_field(&value["url"]),
            "title":value["title"].as_str().map(message_preview),"page_bytes":len(&value["text"]),
            "link_count":count(&value["links"])}),
        "search_web" => {
            let sources: Vec<_> = value["results"]
                .as_array()
                .into_iter()
                .flatten()
                .take(5)
                .map(|item| {
                    json!({"site":origin_field(&item["url"]),
                    "title":item["title"].as_str().map(message_preview)})
                })
                .collect();
            json!({"result_count":count(&value["results"]),"sources":sources})
        }
        "call_mcp" => json!({"response_bytes":len(&value["result"])}),
        "list_mcp_tools" => json!({"tool_count":value["total"].as_u64()
            .unwrap_or_else(||count(&value["shared_tools"]) as u64)}),
        "react_to_message" | "send_task_file" => json!({"queued":value["queued"].as_bool()}),
        "set_goal" => json!({"saved":value["saved"].as_bool()}),
        "delegate" => json!({"helper_replied":len(&value["helper_reply"])>0}),
        "form_submission" => json!({"submitted":value["submitted"].as_bool(),
            "http_status":value["http_status"].as_u64().filter(|status|(100..600).contains(status))}),
        _ => json!({"response_bytes":value.to_string().len()}),
    }
}

pub(super) fn error_category(error: &anyhow::Error) -> &'static str {
    let message = error.to_string().to_lowercase();
    if message.contains("cloudflare requires verification") {
        "site_verification_required"
    } else if message.contains("timed out") || message.contains("too long to respond") {
        "timeout"
    } else if message.contains("worker unavailable") || message.contains("worker is busy") {
        "worker_unavailable"
    } else if message.contains("search providers did not return") {
        "no_search_results"
    } else if message.contains("could not establish an https connection") {
        "https_connection_failed"
    } else if message.contains("website returned http") {
        "site_http_error"
    } else {
        "tool_error"
    }
}
