use super::smtp::{address, valid_message_id};
use crate::store::mail::MailContent;
use mailparse::{DispositionType, MailAddr, MailHeader, MailHeaderMap, ParsedMail};

const MAX_TEXT: usize = 12_000;

pub(super) fn decode(bytes: &[u8], partial: bool) -> MailContent {
    if let Ok(parsed) = mailparse::parse_mail(bytes) {
        return content(&parsed.headers, message_text(&parsed), partial);
    }
    let headers = mailparse::parse_headers(bytes)
        .map(|(headers, _)| headers)
        .unwrap_or_default();
    content(&headers, String::new(), partial)
}

fn content(headers: &[MailHeader<'_>], raw_text: String, partial: bool) -> MailContent {
    let from = mailbox_header(headers, "From");
    let reply_to = mailbox_header(headers, "Reply-To").or_else(|| from.clone());
    let subject = headers
        .get_first_value("Subject")
        .map(|value| single_line(&value, 180))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "(No subject)".into());
    let message_id = headers
        .get_first_value("Message-ID")
        .filter(|id| valid_message_id(id));
    let mut text = raw_text.trim().to_string();
    let truncated = partial || text.len() > MAX_TEXT;
    text.truncate(text.floor_char_boundary(MAX_TEXT));
    if truncated {
        text.push_str("\n\n[Email text truncated.]");
    }
    MailContent {
        from: from.unwrap_or_else(|| "Unknown sender".into()),
        reply_to,
        subject,
        message_id,
        text,
    }
}

fn mailbox_header(headers: &[MailHeader<'_>], name: &str) -> Option<String> {
    let [header] = headers.get_all_headers(name).try_into().ok()?;
    let addresses = mailparse::addrparse_header(header).ok()?;
    let [MailAddr::Single(mailbox)] = addresses.as_slice() else {
        return None;
    };
    address(&mailbox.addr)
        .ok()
        .map(|address| address.to_string())
}

fn single_line(value: &str, max_bytes: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    value[..value.floor_char_boundary(max_bytes)].to_string()
}

fn message_text(message: &ParsedMail<'_>) -> String {
    let mut html = None;
    let mut parts = vec![message];
    while let Some(part) = parts.pop() {
        let disposition = part.get_content_disposition();
        if disposition.disposition != DispositionType::Inline
            || disposition.params.contains_key("filename")
            || part.ctype.params.contains_key("name")
        {
            continue;
        }
        match part.ctype.mimetype.as_str() {
            "text/plain" => {
                if let Ok(value) = part.get_body()
                    && !value.trim().is_empty()
                {
                    return clean_text(value);
                }
            }
            "text/html" => {
                if html.is_none() {
                    html = part
                        .get_body()
                        .ok()
                        .filter(|value| !value.trim().is_empty());
                }
            }
            mime if mime.starts_with("multipart/") => {
                parts.extend(part.subparts.iter().rev());
            }
            _ => {}
        }
    }
    html.and_then(|html| {
        html2text::config::plain_no_decorate()
            .string_from_read(html.as_bytes(), 120)
            .ok()
    })
    .map(clean_text)
    .unwrap_or_default()
}

fn clean_text(value: String) -> String {
    value
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_multipart_body_excludes_attachments() {
        let content = decode(
            concat!(
                "From: Julie Example <julie@example.com>\r\n",
                "Reply-To: help@example.com\r\n",
                "Subject: =?UTF-8?Q?A_=E2=9C=93?=\r\n",
                "Message-ID: <incoming@example.com>\r\n",
                "Content-Type: multipart/mixed; boundary=section\r\n\r\n",
                "--section\r\nContent-Type: text/plain; name=attachment.txt\r\n\r\n",
                "Attachment content\r\n",
                "--section\r\nContent-Type: text/plain; charset=utf-8\r\n",
                "Content-Transfer-Encoding: quoted-printable\r\n\r\nHello =E2=9C=93\r\n",
                "--section--\r\n"
            )
            .as_bytes(),
            false,
        );
        assert_eq!(content.from, "julie@example.com");
        assert_eq!(content.reply_to.as_deref(), Some("help@example.com"));
        assert_eq!(content.subject, "A ✓");
        assert_eq!(
            content.message_id.as_deref(),
            Some("<incoming@example.com>")
        );
        assert_eq!(content.text, "Hello ✓");
    }

    #[test]
    fn html_only_body_excludes_active_elements_and_remote_urls() {
        let content = decode(
            concat!(
                "From: Multiple <one@example.com>, <two@example.com>\r\n",
                "Content-Type: text/html; charset=utf-8\r\n\r\n",
                "<p>Hello &amp; welcome.</p><script>attachment()</script>",
                "<img src='https://private.example.com/pixel'>"
            )
            .as_bytes(),
            false,
        );
        assert_eq!(content.from, "Unknown sender");
        assert!(content.reply_to.is_none());
        assert_eq!(content.text, "Hello & welcome.");
    }

    #[test]
    fn truncated_utf8_body_includes_a_notice() {
        let raw = format!(
            "Content-Type: text/plain; charset=UTF-8\r\n\r\na{}",
            "✓".repeat(9000)
        );
        let content = decode(raw.as_bytes(), true);
        assert!(content.text.ends_with("[Email text truncated.]"));
        assert!(content.text.starts_with("a✓"));
        assert!(content.text.len() < MAX_TEXT + 30);
    }
}
