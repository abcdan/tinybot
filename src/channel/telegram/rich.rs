use super::formatting::{normalized_markdown, public_link};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::fmt::Write;

/// Render GFM tables using only the Telegram rich HTML elements we produce here.
/// Arbitrary model HTML stays literal and Markdown images never become attachments.
pub(super) fn table(body: &str) -> Option<String> {
    if !body.contains('|') {
        return None;
    }
    let markdown = normalized_markdown(body);
    let mut html = String::new();
    let mut has_table = false;
    let mut alignments = Vec::new();
    let mut header = false;
    let mut column = 0;
    let mut linked = false;
    for event in Parser::new_ext(
        &markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::Table(columns)) if columns.len() <= 20 => {
                has_table = true;
                alignments = columns;
                html.push_str("<table bordered compact>");
            }
            Event::Start(Tag::TableHead) => {
                header = true;
                column = 0;
                html.push_str("<tr>");
            }
            Event::End(TagEnd::TableHead) => {
                header = false;
                html.push_str("</tr>");
            }
            Event::Start(Tag::TableRow) => {
                column = 0;
                html.push_str("<tr>");
            }
            Event::End(TagEnd::TableRow) => html.push_str("</tr>"),
            Event::End(TagEnd::Table) => html.push_str("</table>"),
            Event::Start(Tag::TableCell) => {
                html.push_str(if header { "<th" } else { "<td" });
                if let Some(alignment) = alignments.get(column) {
                    html.push_str(match alignment {
                        Alignment::Left => " align=\"left\"",
                        Alignment::Center => " align=\"center\"",
                        Alignment::Right => " align=\"right\"",
                        Alignment::None => "",
                    });
                }
                html.push('>');
                column += 1;
            }
            Event::End(TagEnd::TableCell) => html.push_str(if header { "</th>" } else { "</td>" }),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                escape(&mut html, &text);
            }
            Event::Code(code) => {
                html.push_str("<code>");
                escape(&mut html, &code);
                html.push_str("</code>");
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                linked = public_link(&dest_url).is_some_and(|url| {
                    html.push_str("<a href=\"");
                    escape(&mut html, url.as_str());
                    html.push_str("\">");
                    true
                });
            }
            Event::End(TagEnd::Link) => {
                if linked {
                    html.push_str("</a>");
                }
                linked = false;
            }
            Event::SoftBreak | Event::HardBreak => html.push_str("<br>"),
            Event::Rule => html.push_str("<hr>"),
            Event::Start(Tag::Paragraph | Tag::HtmlBlock) => html.push_str("<p>"),
            Event::End(TagEnd::Paragraph | TagEnd::HtmlBlock) => html.push_str("</p>"),
            Event::Start(Tag::Heading { level, .. }) => write!(html, "<{level}>").ok()?,
            Event::End(TagEnd::Heading(level)) => write!(html, "</{level}>").ok()?,
            Event::Start(Tag::Strong) => html.push_str("<b>"),
            Event::End(TagEnd::Strong) => html.push_str("</b>"),
            Event::Start(Tag::Emphasis) => html.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => html.push_str("</i>"),
            Event::Start(Tag::Strikethrough) => html.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => html.push_str("</s>"),
            Event::Start(Tag::BlockQuote(_)) => html.push_str("<blockquote>"),
            Event::End(TagEnd::BlockQuote(_)) => html.push_str("</blockquote>"),
            Event::Start(Tag::CodeBlock(_)) => html.push_str("<pre>"),
            Event::End(TagEnd::CodeBlock) => html.push_str("</pre>"),
            Event::Start(Tag::List(None)) => html.push_str("<ul>"),
            Event::Start(Tag::List(Some(start))) => write!(html, "<ol start=\"{start}\">").ok()?,
            Event::End(TagEnd::List(true)) => html.push_str("</ol>"),
            Event::End(TagEnd::List(false)) => html.push_str("</ul>"),
            Event::Start(Tag::Item) => html.push_str("<li>"),
            Event::End(TagEnd::Item) => html.push_str("</li>"),
            _ => return None,
        }
    }
    has_table.then_some(html)
}

fn escape(html: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '"' => html.push_str("&quot;"),
            _ => html.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::table;

    #[test]
    fn rich_table_formats_headers_and_keeps_model_markup_inert() {
        let body = "### Prijzen\n\n| Abonnement | Prijs |\n| --- | ---: |\n\
                    | **Lite** | €15 |\n\n\
                    <tg-map lat=\"41.9\" long=\"12.5\" zoom=\"14\"/>\n\n\
                    Bronnen: [Docs](https://example.com/?x=1&y=2) [Unsafe](javascript:alert(1))";
        let html = table(body).expect("expected native rich table");
        assert!(html.contains("<h3>Prijzen</h3>"));
        assert!(html.contains("<table bordered compact><tr><th>Abonnement</th>"));
        assert!(html.contains("<td><b>Lite</b></td><td align=\"right\">€15</td>"));
        assert!(html.contains("&lt;tg-map lat=&quot;41.9&quot;"));
        assert!(html.contains("<a href=\"https://example.com/?x=1&amp;y=2\">Docs</a> Unsafe"));
        assert!(!html.contains("<tg-map"));
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn only_real_tables_use_rich_delivery_without_embedded_images() {
        assert!(table("`foo | bar`").is_none());
        assert!(table("```\n| Name | Cost |\n| --- | --- |\n| Lite | €15 |\n```").is_none());
        assert!(table("| Name |\n| --- |\n| ![image](https://example.com/image.png) |").is_none());
    }
}
