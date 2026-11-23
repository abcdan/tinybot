use crate::security::clip;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    title: String,
    text: String,
    items: Vec<String>,
}

impl Card {
    pub fn new(title: &str, text: &str, items: &[String]) -> Option<Self> {
        let card = Self {
            title: clean(title, 120),
            text: clean(text, 850),
            items: items
                .iter()
                .take(4)
                .map(|item| clean(item, 120))
                .filter(|item| !item.is_empty())
                .collect(),
        };
        (!card.title.is_empty() || !card.text.is_empty() || !card.items.is_empty()).then_some(card)
    }

    pub fn plain_text(&self) -> String {
        let items = self
            .items
            .iter()
            .map(|item| format!("• {item}"))
            .collect::<Vec<_>>()
            .join("\n");
        [self.title.as_str(), self.text.as_str(), items.as_str()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn html(&self) -> String {
        let mut html = String::from(
            r#"<style>
body { margin:0; padding:12px 14px calc(12px + env(safe-area-inset-bottom)); }
h4, p, ul { margin:0; }
h4 + p, p + ul, h4 + ul { margin-top:8px; }
p, li { white-space:pre-wrap; overflow-wrap:anywhere; }
ul { padding-inline-start:20px; }
</style>"#,
        );
        if !self.title.is_empty() {
            html.push_str(&format!("<h4>{}</h4>", escape(&self.title)));
        }
        if !self.text.is_empty() {
            html.push_str(&format!("<p>{}</p>", escape(&self.text)));
        }
        if !self.items.is_empty() {
            html.push_str("<ul>");
            for item in &self.items {
                html.push_str(&format!("<li>{}</li>", escape(item)));
            }
            html.push_str("</ul>");
        }
        html
    }
}

fn clean(text: &str, limit: usize) -> String {
    let text: String = text
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect();
    clip(text.trim(), limit).trim_end().to_owned()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
