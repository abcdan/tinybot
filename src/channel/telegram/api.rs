use super::{Telegram, formatting};
use crate::{channel::MessageDelivery, net, store};
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) enum WidgetDelivery {
    Sent(Option<i64>),
    Retry(i64),
    Rejected,
}

impl Telegram {
    pub fn new(token: String, origin: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(45))
                .build()?,
            api: format!("https://api.telegram.org/bot{token}"),
            origin,
        })
    }

    pub(super) async fn feedback(&self, method: &str, data: Value) -> Result<Option<Duration>> {
        let output = tokio::time::timeout(Duration::from_secs(3), async {
            let mut data = data;
            let output = self.call(method, data.clone()).await?;
            if data.get("entities").is_some()
                && bad_request(&output).is_some_and(|description| invalid_entities(&description))
            {
                data.as_object_mut()
                    .expect("feedback object")
                    .remove("entities");
                return self.call(method, data).await;
            }
            Ok(output)
        })
        .await??;
        if output["ok"] == true {
            return Ok(None);
        }
        if let Some(seconds) = output["parameters"]["retry_after"].as_u64() {
            return Ok(Some(Duration::from_secs(seconds.clamp(1, 600))));
        }
        anyhow::bail!("Telegram rejected the chat feedback")
    }

    pub(in crate::channel) async fn call(&self, method: &str, data: Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}/{method}", self.api))
            .json(&data)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Telegram request failed"))?;
        Ok(serde_json::from_slice(
            &net::body(response, 2_000_000).await?,
        )?)
    }

    pub(super) async fn send_widget(&self, kind: &str, data: Value) -> Result<WidgetDelivery> {
        let response = self.call("sendWidget", data).await?;
        if response["ok"] == true {
            tracing::debug!(
                widget_kind = kind,
                has_widget = response["result"]["widget"].is_object(),
                "Telegram accepted widget message"
            );
            return Ok(WidgetDelivery::Sent(accepted_message_id(&response)));
        }
        if let Some(delay) = retry_after(&response) {
            return Ok(WidgetDelivery::Retry(delay));
        }
        let error_code = response["error_code"].as_i64();
        let category = widget_rejection(&response);
        tracing::warn!(
            widget_kind = kind,
            ?error_code,
            category,
            "Telegram rejected sendWidget"
        );
        Ok(WidgetDelivery::Rejected)
    }

    pub(super) async fn send_message(&self, data: Value) -> Result<MessageDelivery> {
        message_result(&self.call("sendMessage", data).await?)
    }

    pub(super) async fn send_reply(
        &self,
        user: i64,
        body: &str,
        keyboard: Option<Value>,
        reply_to: Option<i64>,
    ) -> Result<MessageDelivery> {
        let reply = formatting::render(body);
        let mut data =
            json!({"chat_id":user,"text":reply.text,"link_preview_options":{"is_disabled":true}});
        add_reply_anchor(&mut data, reply_to);
        if let Some(keyboard) = &keyboard {
            data["reply_markup"] = keyboard.clone();
        }
        let has_entities = !reply.entities.is_empty();
        if has_entities {
            data["entities"] = json!(reply.entities);
        }
        let has_links = !reply.links.is_empty();
        if has_links {
            let mut markup = keyboard
                .clone()
                .unwrap_or_else(|| json!({"inline_keyboard":[]}));
            if let Some(rows) = markup["inline_keyboard"].as_array_mut() {
                rows.extend(reply.links.iter().map(|link| json!([link])));
            }
            data["reply_markup"] = markup;
        }
        let response = self.call("sendMessage", data.clone()).await?;
        if let Some(description) = bad_request(&response) {
            if has_entities && invalid_entities(&description) {
                data.as_object_mut()
                    .expect("message object")
                    .remove("entities");
                return self.send_message(data).await;
            }
            if has_links
                && (description.contains("button_url_invalid")
                    || description.contains("button url"))
            {
                if let Some(keyboard) = keyboard {
                    data["reply_markup"] = keyboard;
                } else {
                    data.as_object_mut()
                        .expect("message object")
                        .remove("reply_markup");
                }
                return self.send_message(data).await;
            }
        }
        message_result(&response)
    }
}

fn accepted_message_id(response: &Value) -> Option<i64> {
    response["result"]["message_id"]
        .as_i64()
        .filter(|id| *id > 0)
}

pub(super) fn add_reply_anchor(data: &mut Value, reply_to: Option<i64>) {
    if let Some(message_id) = reply_to.filter(|id| *id > 0) {
        data["reply_parameters"] =
            json!({"message_id":message_id,"allow_sending_without_reply":true});
    }
}

fn message_result(response: &Value) -> Result<MessageDelivery> {
    if response["ok"] == true {
        return Ok(MessageDelivery::Sent(accepted_message_id(response)));
    }
    if let Some(delay) = retry_after(response) {
        return Ok(MessageDelivery::Retry(delay));
    }
    anyhow::bail!("Telegram rejected the reply")
}

fn bad_request(response: &Value) -> Option<String> {
    (response["ok"] == false && response["error_code"] == 400 && retry_after(response).is_none())
        .then(|| {
            response["description"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase()
        })
}

fn invalid_entities(description: &str) -> bool {
    description.contains("parse entities") || description.contains("entity bounds")
}

fn retry_after(response: &Value) -> Option<i64> {
    response["parameters"]["retry_after"]
        .as_i64()
        .map(|delay| delay.clamp(1, 600))
}

pub(in crate::channel) fn widget_rejection(response: &Value) -> &'static str {
    let description = response["description"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if description.contains("input_media_widget_not_allowed") {
        "widget_access_restricted"
    } else if description.contains("widget_html") || description.contains("widget_too_large") {
        "invalid_widget_html"
    } else if description.contains("widget_data") {
        "invalid_widget_data"
    } else if description.contains("button") || description.contains("reply markup") {
        "invalid_widget_keyboard"
    } else if response["error_code"] == 404 {
        "method_unavailable"
    } else {
        "other"
    }
}

pub(in crate::channel) fn suggested_keyboard(replies: &[store::SuggestedReply]) -> Value {
    let buttons: Vec<_> = replies
        .iter()
        .map(|reply| [json!({"text":reply.body,"callback_data":format!("reply/{}",reply.token)})])
        .collect();
    json!({"inline_keyboard":buttons})
}
