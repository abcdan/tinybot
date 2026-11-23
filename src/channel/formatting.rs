use crate::security::clip;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::cmp::Reverse;
use url::{Host, Url};

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
pub(super) struct SourceButton {
    text: String,
    url: String,
}

pub(super) struct Reply {
    pub text: String,
    pub entities: Vec<Entity>,
    pub sources: Vec<SourceButton>,
}

pub(super) fn render(body: &str) -> Reply {
    let body = clip(body, 3900);
    let (markdown, sources) = if let Some(start) = source_start(body) {
        let prefix = body[..start].trim_end();
        let separator = if prefix.is_empty() { "" } else { "\n\n" };
        (
            format!("{prefix}{separator}{}", body[start..].trim_start()),
            prefix.len() + separator.len(),
        )
    } else {
        (body.to_owned(), usize::MAX)
    };
    let mut output = Renderer::default();
    for (event, range) in
        Parser::new_ext(&markdown, Options::ENABLE_STRIKETHROUGH).into_offset_iter()
    {
        output.render(event, range.start >= sources);
    }
    if output.text.trim().is_empty() || output.text.encode_utf16().count() > 4096 {
        return Reply {
            text: body.to_owned(),
            entities: Vec::new(),
            sources: Vec::new(),
        };
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
        sources: output.sources,
    }
}

fn source_start(body: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let mut in_code = false;
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code = true,
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Text(_) if !in_code => {
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
    sources: Vec<SourceButton>,
    active: Vec<Span>,
    lists: Vec<Option<u64>>,
    breaks: usize,
    code_block: bool,
    link: bool,
    quotes: usize,
}

struct Span {
    kind: &'static str,
    start: usize,
    url: Option<String>,
    language: Option<String>,
    source: bool,
}

impl Renderer {
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
            source: false,
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
            let citation = label.trim_matches(['[', ']']);
            if span.source
                || (!citation.is_empty() && citation.bytes().all(|byte| byte.is_ascii_digit()))
            {
                let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
                self.source(url, &label);
            }
        }
        self.entities.push(Entity {
            kind: span.kind,
            offset: self.text[..span.start].encode_utf16().count(),
            length: self.text[span.start..].encode_utf16().count(),
            url: span.url,
            language: span.language,
        });
    }

    fn source(&mut self, url: &str, label: &str) {
        if !label.is_empty()
            && self.sources.len() < 4
            && self.sources.iter().all(|button| button.url != url)
        {
            self.sources.push(SourceButton {
                text: clip(label, 64).to_owned(),
                url: url.to_owned(),
            });
        }
    }

    fn render(&mut self, event: Event<'_>, sources: bool) {
        match event {
            Event::Text(text) => {
                self.push(&text);
                if sources && !self.code_block && !self.link {
                    for word in text.split_whitespace() {
                        if let Some(url) =
                            public_link(word.trim_matches(['(', ')', '<', '>', ',', '.', ';']))
                        {
                            self.source(url.as_str(), url.host_str().expect("checked host"));
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
                    span.source = sources;
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

fn public_link(value: &str) -> Option<Url> {
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
    fn puts_citation_tail_on_its_own_block_and_only_adds_source_buttons() {
        let reply = render(
            "Bekijk [de site](https://example.com/start). **Bronnen:**\n\
             - [Documentatie](https://example.com/docs)\n- https://example.org/guide",
        );
        assert_eq!(
            reply.text,
            "Bekijk de site.\n\nBronnen:\n\n• Documentatie\n• https://example.org/guide"
        );
        assert_eq!(
            json!(reply.sources),
            json!([
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
            json!(reply.sources),
            json!([{"text":"example.org","url":"https://example.org/doc"}])
        );

        let reply = render(
            "Antwoord.\n\nSources:\n[One](https://example.org/one)\n[Two](https://example.org/two)",
        );
        assert_eq!(reply.text, "Antwoord.\n\nSources:\nOne\nTwo");
        assert_eq!(reply.sources.len(), 2);
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
        assert!(reply.sources.is_empty());
    }

    #[test]
    fn filters_unsafe_destinations_and_keeps_plain_markdown_links_inline() {
        let reply = render(
            "[Gewoon](https://example.com/) [Ongeldig](javascript:alert(1)) [Inlog](https://user@example.com/)",
        );
        assert_eq!(reply.text, "Gewoon Ongeldig Inlog");
        assert_eq!(reply.entities.len(), 1);
        assert_eq!(
            reply.entities[0].url.as_deref(),
            Some("https://example.com/")
        );
        assert!(reply.sources.is_empty());

        let reply = render(
            "Sources:\n[Localhost](https://localhost/a) [Subdomain](https://app.localhost./a) \
             [Private](https://192.168.1.5/a) [Mapped](https://[::ffff:127.0.0.1]/a)",
        );
        assert!(reply.entities.is_empty());
        assert!(reply.sources.is_empty());
    }
}
