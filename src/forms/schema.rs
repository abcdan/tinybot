use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebsiteForm {
    pub page: String,
    pub destination: String,
    pub fingerprint: String,
    pub index: usize,
    pub fields: Vec<Field>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormSpec {
    pub fields: Vec<Field>,
}

impl FormSpec {
    pub fn widget_choice(&self) -> Option<&Field> {
        let [field] = self.fields.as_slice() else {
            return None;
        };
        // One question with short options fits Telegram's 1024-byte query response,
        // including the request ID and JSON escaping.
        (field.kind == "select"
            && !field.private
            && (2..=6).contains(&field.options.len())
            && field.label.len() + field.options.iter().map(String::len).sum::<usize>() <= 420
            && !field.label.trim().is_empty()
            && !field.label.chars().any(char::is_control)
            && field
                .options
                .iter()
                .all(|option| !option.trim().is_empty() && !option.chars().any(char::is_control)))
        .then_some(field)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=24).contains(&self.fields.len()),
            "form has an unsupported field count"
        );
        let mut names = BTreeSet::new();
        for field in &self.fields {
            ensure!(
                !field.name.is_empty() && field.name.len() <= 96 && names.insert(&field.name),
                "form field names must be unique"
            );
            ensure!(
                !field.label.is_empty() && field.label.len() <= 160,
                "invalid form label"
            );
            ensure!(
                field.hint.as_ref().is_none_or(|hint| hint.len() <= 160),
                "invalid field hint"
            );
            ensure!(
                matches!(
                    field.kind.as_str(),
                    "text"
                        | "textarea"
                        | "email"
                        | "tel"
                        | "date"
                        | "number"
                        | "select"
                        | "checkbox"
                ),
                "unknown form field type"
            );
            ensure!(
                field.options.len() <= 50 && field.options.iter().all(|value| value.len() <= 160),
                "invalid form options"
            );
            ensure!(
                field.kind != "select" || !field.options.is_empty(),
                "select requires options"
            );
        }
        Ok(())
    }

    pub fn match_website(self, observed: &WebsiteForm) -> Result<Vec<Field>> {
        self.validate()?;
        for field in &self.fields {
            let original = observed
                .fields
                .iter()
                .find(|original| original.name == field.name)
                .context("review added a field absent from the website")?;
            ensure!(
                original.kind == field.kind
                    && original.options == field.options
                    && (!original.required || field.required)
                    && (!original.private || field.private),
                "review changed a browser constraint"
            );
        }
        ensure!(
            observed
                .fields
                .iter()
                .filter(|field| field.required)
                .all(|field| self
                    .fields
                    .iter()
                    .any(|proposed| proposed.name == field.name)),
            "review omitted a required field"
        );
        Ok(self.fields)
    }
}

pub(super) fn validate_values(fields: &[Field], values: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        values.len() <= fields.len() && values.values().map(String::len).sum::<usize>() <= 24_000,
        "invalid form data"
    );
    for name in values.keys() {
        ensure!(
            fields.iter().any(|field| field.name == *name),
            "unexpected form field"
        );
    }
    for field in fields {
        let value = values.get(&field.name).map(String::as_str).unwrap_or("");
        ensure!(
            value.len() <= 4096 && !value.contains('\0') && (!field.required || !value.is_empty()),
            "invalid required field"
        );
        if value.is_empty() {
            continue;
        }
        let valid = match field.kind.as_str() {
            "select" => field.options.iter().any(|option| option == value),
            "checkbox" => matches!(value, "true" | "false") && (!field.required || value == "true"),
            "number" => value.parse::<f64>().is_ok_and(f64::is_finite),
            "email" => value.split_once('@').is_some_and(|(name, host)| {
                !name.is_empty() && host.contains('.') && !value.contains(char::is_whitespace)
            }),
            "date" => {
                value.len() == 10
                    && value.bytes().enumerate().all(|(i, b)| {
                        if matches!(i, 4 | 7) {
                            b == b'-'
                        } else {
                            b.is_ascii_digit()
                        }
                    })
            }
            _ => true,
        };
        ensure!(valid, "invalid form value");
    }
    Ok(())
}
