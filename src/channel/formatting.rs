use crate::security::clip;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::cmp::Reverse;
use url::{Host, Url};

const MAX_MESSAGE_UNITS: usize = 4096;

#[derive(Serialize)]
pub(super) struct Entity {
    #[serde(rename = "type")]
    kind: &'static str,
    offset: usize,
    length: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

#[derive(Serialize)]
pub(super) struct LinkButton {
    text: String,
    url: String,
}

pub(super) struct Reply {
    pub text: String,
    pub entities: Vec<Entity>,
    pub links: Vec<LinkButton>,
}

pub(super) fn render(body: &str) -> Reply {
    let markdown = normalized_markdown(body);
    let mut output = Renderer::markdown(&markdown, true);
    if output.has_table && output.text.encode_utf16().count() > MAX_MESSAGE_UNITS {
        output = Renderer::markdown(&markdown, false);
    }
    let oversized = output.text.encode_utf16().count() > MAX_MESSAGE_UNITS;
    if output.text.trim().is_empty() || (!output.has_table && oversized) {
        return Reply {
            text: clip(body, 3900).to_owned(),
            entities: Vec::new(),
            links: Vec::new(),
        };
    }
    if oversized {
        // Keep the rendered table readable even when its compact form is too long.
        let mut units = 0;
        let end = output
            .text
            .char_indices()
            .find_map(|(index, character)| {
                units += character.len_utf16();
                (units >= MAX_MESSAGE_UNITS).then_some(index)
            })
            .expect("text exceeds Telegram's limit");
        output.text.truncate(end);
        let units = output.text.encode_utf16().count();
        output
            .entities
            .retain(|entity| entity.offset + entity.length <= units);
        output.text.push('…');
    }
    // Telegram forbids overlapping styles on code entities and nested block quotes.
    let code: Vec<_> = output
        .entities
        .iter()
        .filter(|entity| matches!(entity.kind, "pre" | "code"))
        .map(|entity| (entity.offset, entity.offset + entity.length))
        .collect();
    output.entities.retain(|entity| {
        matches!(entity.kind, "pre" | "code")
            || !code
                .iter()
                .any(|&(start, end)| entity.offset < end && start < entity.offset + entity.length)
    });
    output
        .entities
        .sort_by_key(|entity| (entity.offset, Reverse(entity.length)));
    output.entities.dedup_by(|left, right| {
        left.kind == right.kind && left.offset == right.offset && left.length == right.length
    });
    output.entities.truncate(100);
    Reply {
        text: output.text,
        entities: output.entities,
        links: output.links,
    }
}

pub(super) fn normalized_markdown(body: &str) -> String {
    let body = clip(body, 3900);
    if let Some(start) = source_start(body) {
        let prefix = body[..start].trim_end();
        let separator = if prefix.is_empty() { "" } else { "\n\n" };
        format!("{prefix}{separator}{}", body[start..].trim_start())
    } else {
        body.to_owned()
    }
}

fn source_start(body: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let mut in_code = false;
    let mut in_table = false;
    for (event, range) in Parser::new_ext(body, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code = true,
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Start(Tag::Table(_)) => in_table = true,
            Event::End(TagEnd::Table) => in_table = false,
            Event::Text(_) if !in_code && !in_table => {
                for label in [
                    "sources",
                    "source",
                    "bronnen",
                    "bron",
                    "references",
                    "referenties",
                ] {
                    for (offset, _) in lower[range.clone()].match_indices(label) {
                        let start = range.start + offset;
                        let tail = &lower[start + label.len()..];
                        let suffix = tail
                            .strip_prefix("**")
                            .or_else(|| tail.strip_prefix("__"))
                            .unwrap_or(tail);
                        if !suffix.starts_with(':') && !suffix.starts_with(['\n', '\r']) {
                            continue;
                        }
                        if !tail.contains("https://") {
                            continue;
                        }
                        let prefix = &body[..start];
                        let trimmed = prefix.trim_end_matches([' ', '\t']);
                        let heading = trimmed
                            .strip_suffix("**")
                            .or_else(|| trimmed.strip_suffix("__"))
                            .unwrap_or(trimmed);
                        let line = heading.rsplit('\n').next()?.trim();
                        if line.chars().all(|character| character == '#') {
                            return Some(heading.rfind('\n').map_or(0, |position| position + 1));
                        }
                        if line.ends_with(['.', '!', '?']) {
                            return Some(heading.len());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Default)]
struct Renderer {
    text: String,
    entities: Vec<Entity>,
    links: Vec<LinkButton>,
    active: Vec<Span>,
    lists: Vec<Option<u64>>,
    breaks: usize,
    code_block: bool,
    link: bool,
    quotes: usize,
    has_table: bool,
}

struct Span {
    kind: &'static str,
    start: usize,
    url: Option<String>,
    language: Option<String>,
}

impl Renderer {
    fn markdown(body: &str, labelled_rows: bool) -> Self {
        let mut output = Self::default();
        let mut events =
            Parser::new_ext(body, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES);
        while let Some(event) = events.next() {
            if matches!(event, Event::Start(Tag::Table(_))) {
                output.table(&mut events, labelled_rows);
            } else {
                output.render(event);
            }
        }
        output
    }

    fn table<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>, labelled_rows: bool) {
        let mut headings = Vec::new();
        let mut cells = Vec::new();
        let mut rows = Vec::new();
        while let Some(event) = events.next() {
            match event {
                Event::Start(Tag::TableCell) => cells.push(
                    events
                        .by_ref()
                        .take_while(|event| !matches!(event, Event::End(TagEnd::TableCell)))
                        .collect::<Vec<_>>(),
                ),
                Event::End(TagEnd::TableHead) => headings = std::mem::take(&mut cells),
                Event::End(TagEnd::TableRow) => rows.push(std::mem::take(&mut cells)),
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }

        self.has_table = true;
        self.breaks = self.breaks.max(2);
        if labelled_rows && !rows.is_empty() {
            for row in rows {
                for (column, cell) in row.into_iter().enumerate() {
                    if let Some(heading) =
                        headings.get(column).filter(|heading| !heading.is_empty())
                    {
                        self.open("bold");
                        heading.iter().cloned().for_each(|event| self.render(event));
                        self.close("bold");
                        self.push(" · ");
                    }
                    if cell.is_empty() {
                        self.push("—");
                    }
                    cell.into_iter().for_each(|event| self.render(event));
                    self.breaks = 1;
                }
                self.breaks = 2;
                // Three UTF-8 bytes per UTF-16 unit is the largest possible ratio.
                // The caller restarts in compact mode once a labelled table is oversized.
                if self.text.len() > 3 * MAX_MESSAGE_UNITS {
                    break;
                }
            }
        } else {
            self.open("bold");
            self.compact_table_row(headings);
            self.close("bold");
            for row in rows {
                self.breaks = 1;
                self.compact_table_row(row);
            }
        }
        self.breaks = 2;
    }

    fn compact_table_row(&mut self, cells: Vec<Vec<Event<'_>>>) {
        for (column, cell) in cells.into_iter().enumerate() {
            if column != 0 {
                self.push(" · ");
            }
            if cell.is_empty() {
                self.push("—");
            }
            cell.into_iter().for_each(|event| self.render(event));
        }
    }

    fn flush(&mut self) {
        if !self.text.is_empty() {
            let existing = self
                .text
                .bytes()
                .rev()
                .take_while(|byte| *byte == b'\n')
                .count();
            for _ in existing..self.breaks {
                self.text.push('\n');
            }
        }
        self.breaks = 0;
    }

    fn push(&mut self, text: &str) {
        if !text.is_empty() {
            self.flush();
            self.text.push_str(text);
        }
    }

    fn open(&mut self, kind: &'static str) -> &mut Span {
        self.flush();
        self.active.push(Span {
            kind,
            start: self.text.len(),
            url: None,
            language: None,
        });
        self.active.last_mut().expect("span just opened")
    }

    fn close(&mut self, kind: &'static str) {
        let Some(index) = self.active.iter().rposition(|span| span.kind == kind) else {
            return;
        };
        let span = self.active.remove(index);
        let label = &self.text[span.start..];
        if label.is_empty() {
            return;
        }
        if let Some(url) = span.url.as_deref() {
            let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
            self.link_button(url, &label);
        }
        self.entities.push(Entity {
            kind: span.kind,
            offset: self.text[..span.start].encode_utf16().count(),
            length: self.text[span.start..].encode_utf16().count(),
            url: span.url,
            language: span.language,
        });
    }

    fn link_button(&mut self, url: &str, label: &str) {
        if !label.is_empty()
            && self.links.len() < 4
            && self.links.iter().all(|button| button.url != url)
        {
            self.links.push(LinkButton {
                text: clip(label, 64).to_owned(),
                url: url.to_owned(),
            });
        }
    }

    fn render(&mut self, event: Event<'_>) {
        match event {
            Event::Text(text) => {
                self.push(&text);
                if !self.code_block && !self.link {
                    for word in text.split_whitespace() {
                        if let Some(url) =
                            public_link(word.trim_matches(['(', ')', '<', '>', ',', '.', ';']))
                        {
                            self.link_button(url.as_str(), url.host_str().expect("checked host"));
                        }
                    }
                }
            }
            Event::Html(text) | Event::InlineHtml(text) => self.push(&text),
            Event::Code(text) => {
                self.open("code");
                self.push(&text);
                self.close("code");
            }
            Event::SoftBreak | Event::HardBreak => self.push("\n"),
            Event::Start(Tag::Heading { .. } | Tag::Strong) => {
                self.open("bold");
            }
            Event::End(TagEnd::Heading(_)) => {
                self.close("bold");
                self.breaks = 2;
            }
            Event::End(TagEnd::Strong) => self.close("bold"),
            Event::Start(Tag::Emphasis) => {
                self.open("italic");
            }
            Event::End(TagEnd::Emphasis) => self.close("italic"),
            Event::Start(Tag::Strikethrough) => {
                self.open("strikethrough");
            }
            Event::End(TagEnd::Strikethrough) => self.close("strikethrough"),
            Event::Start(Tag::BlockQuote(_)) => {
                if self.quotes == 0 {
                    self.breaks = self.breaks.max(2);
                    self.open("blockquote");
                }
                self.quotes += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.quotes -= 1;
                if self.quotes == 0 {
                    self.close("blockquote");
                    self.breaks = 2;
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.breaks = self.breaks.max(2);
                self.code_block = true;
                let span = self.open("pre");
                if let CodeBlockKind::Fenced(info) = kind
                    && let Some(language) = info.split_whitespace().next()
                    && language.len() <= 32
                    && language
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"+#_-".contains(&byte))
                {
                    span.language = Some(language.to_owned());
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                self.close("pre");
                self.code_block = false;
                self.breaks = 2;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link = true;
                if let Some(url) = public_link(&dest_url) {
                    let span = self.open("text_link");
                    span.url = Some(url.into());
                }
            }
            Event::End(TagEnd::Link) => {
                self.close("text_link");
                self.link = false;
            }
            Event::Start(Tag::List(start)) => self.lists.push(start),
            Event::Start(Tag::Item) => {
                let marker = match self.lists.last_mut() {
                    Some(Some(index)) => {
                        let marker = format!("{index}. ");
                        *index += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.push(&marker);
            }
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                self.breaks = if self.lists.is_empty() { 2 } else { 1 };
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                self.breaks = if self.lists.is_empty() { 2 } else { 1 };
            }
            Event::Rule => {
                self.push("────────");
                self.breaks = 2;
            }
            _ => {}
        }
    }
}

pub(super) fn public_link(value: &str) -> Option<Url> {
    if value.len() > 2048 {
        return None;
    }
    let url = Url::parse(value).ok()?;
    let public_host = match url.host()? {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            domain != "localhost" && !domain.ends_with(".localhost")
        }
        Host::Ipv4(address) => crate::net::public(address.into()),
        Host::Ipv6(address) => crate::net::public(address.into()),
    };
    (url.scheme() == "https"
        && public_host
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_markdown_to_utf16_entities_and_keeps_html_as_text() {
        let reply = render("😀 **vet**, *cursief*, ~~weg~~. `<b>code</b>` en <i>letterlijk</i>.");
        assert_eq!(
            reply.text,
            "😀 vet, cursief, weg. <b>code</b> en <i>letterlijk</i>."
        );
        let entities = json!(reply.entities);
        assert_eq!(entities[0], json!({"type":"bold","offset":3,"length":3}));
        assert_eq!(entities[1]["type"], "italic");
        assert_eq!(entities[2]["type"], "strikethrough");
        assert_eq!(entities[3]["type"], "code");
    }

    #[test]
    fn separates_citations_and_adds_buttons_for_reply_links() {
        let reply = render(
            "Bekijk [de site](https://example.com/start). **Bronnen:**\n\
             - [Documentatie](https://example.com/docs)\n- https://example.org/guide",
        );
        assert_eq!(
            reply.text,
            "Bekijk de site.\n\nBronnen:\n\n• Documentatie\n• https://example.org/guide"
        );
        assert_eq!(
            json!(reply.links),
            json!([
                {"text":"de site","url":"https://example.com/start"},
                {"text":"Documentatie","url":"https://example.com/docs"},
                {"text":"example.org","url":"https://example.org/guide"},
            ])
        );
        assert_eq!(
            reply
                .entities
                .iter()
                .filter(|entity| entity.kind == "text_link")
                .count(),
            2
        );
    }

    #[test]
    fn formats_source_headings_and_plain_lookup_fallbacks() {
        let reply = render("An excerpt.\nSource: https://example.org/doc");
        assert_eq!(reply.text, "An excerpt.\n\nSource: https://example.org/doc");
        assert_eq!(
            json!(reply.links),
            json!([{"text":"example.org","url":"https://example.org/doc"}])
        );

        let reply = render(
            "Antwoord.\n\nSources:\n[One](https://example.org/one)\n[Two](https://example.org/two)",
        );
        assert_eq!(reply.text, "Antwoord.\n\nSources:\nOne\nTwo");
        assert_eq!(reply.links.len(), 2);
    }

    #[test]
    fn renders_markdown_tables_as_readable_rows_with_citation_buttons() {
        let reply = render(concat!(
            "Tarieven excl. btw.\n\n",
            "| Abonnement | Maandelijks betalen | Jaarlijks betalen |\n",
            "|---|---:|---:|\n",
            "| 😀 Lite | €25 per maand | €180 per jaar |\n",
            "| **Premium** | €69 per maand | €708 per jaar |\n",
            "\nBronnen:\n[Plug&Pay-tarieven](https://example.com/tarieven)",
        ));
        assert_eq!(
            reply.text,
            concat!(
                "Tarieven excl. btw.\n\n",
                "Abonnement · 😀 Lite\nMaandelijks betalen · €25 per maand\nJaarlijks betalen · €180 per jaar\n\n",
                "Abonnement · Premium\nMaandelijks betalen · €69 per maand\nJaarlijks betalen · €708 per jaar\n\n",
                "Bronnen:\nPlug&Pay-tarieven",
            )
        );
        assert_eq!(
            json!(reply.links),
            json!([{"text":"Plug&Pay-tarieven","url":"https://example.com/tarieven"}])
        );
        let start = reply.text.find("Premium").unwrap();
        let offset = reply.text[..start].encode_utf16().count();
        assert!(reply.entities.iter().any(|entity| {
            entity.kind == "bold" && entity.offset == offset && entity.length == "Premium".len()
        }));
    }

    #[test]
    fn preserves_table_cell_markup_and_does_not_parse_fenced_tables() {
        let reply = render(concat!(
            "| Feature | Details |\n|---|---|\n",
            "| `a\\|b` | [Open](https://example.com/details) |\n",
            "| — | |\n",
            "\n```text\n| Example | Literal |\n|---|---|\n```",
        ));
        assert_eq!(
            reply.text,
            "Feature · a|b\nDetails · Open\n\nFeature · —\nDetails · —\n\n| Example | Literal |\n|---|---|\n"
        );
        assert!(reply.entities.iter().any(|entity| entity.kind == "code"));
        assert!(reply.entities.iter().any(|entity| entity.kind == "pre"));
        assert_eq!(reply.links.len(), 1);
    }

    #[test]
    fn uses_compact_rows_when_repeating_headers_would_exceed_telegram_limit() {
        let header = "Annual subscription paid monthly ".repeat(4);
        let table = format!(
            "| Plan | {} |\n|---|---|\n{}\nSources:\n[Pricing](https://example.com/prices)",
            header.trim_end(),
            "| Lite | €25 |\n".repeat(40),
        );
        let reply = render(&table);
        assert_eq!(reply.text.matches(header.trim_end()).count(), 1);
        assert_eq!(reply.text.matches("Lite · €25").count(), 40);
        assert!(reply.text.ends_with("\n\nSources:\nPricing"));
        assert_eq!(reply.links.len(), 1);
    }

    #[test]
    fn preserves_fenced_code_and_does_not_promote_it_to_markup_or_citations() {
        let reply = render(
            "Voorbeeld:\n\n```rust\n// Sources: https://example.org/\nlet x = \"**bold**\";\n```\n\nNa het voorbeeld.",
        );
        assert!(
            reply
                .text
                .contains("// Sources: https://example.org/\nlet x = \"**bold**\";")
        );
        assert_eq!(reply.entities.len(), 1);
        assert_eq!(reply.entities[0].kind, "pre");
        assert_eq!(reply.entities[0].language.as_deref(), Some("rust"));
        assert!(reply.links.is_empty());
    }

    #[test]
    fn filters_unsafe_destinations_and_keeps_valid_links_inline_and_in_buttons() {
        let reply = render(
            "[Gewoon](https://example.com/) [Ongeldig](javascript:alert(1)) [Inlog](https://user@example.com/)",
        );
        assert_eq!(reply.text, "Gewoon Ongeldig Inlog");
        assert_eq!(reply.entities.len(), 1);
        assert_eq!(
            reply.entities[0].url.as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            json!(reply.links),
            json!([{"text":"Gewoon","url":"https://example.com/"}])
        );

        let reply = render(
            "Sources:\n[Localhost](https://localhost/a) [Subdomain](https://app.localhost./a) \
             [Private](https://192.168.1.5/a) [Mapped](https://[::ffff:127.0.0.1]/a)",
        );
        assert!(reply.entities.is_empty());
        assert!(reply.links.is_empty());
    }

    #[test]
    fn standalone_note_links_become_buttons_without_a_source_heading() {
        let reply = render(
            "Found the note.\n\n[Open the note](https://example.org/notes/1)\n\nRelated: https://example.org/notes/2",
        );
        assert_eq!(
            json!(reply.links),
            json!([
                {"text":"Open the note","url":"https://example.org/notes/1"},
                {"text":"example.org","url":"https://example.org/notes/2"},
            ])
        );
    }
}
