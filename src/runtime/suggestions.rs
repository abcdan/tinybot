use serde_json::Value;

pub(super) fn valid_suggestions(value: &Value) -> Vec<&str> {
    let mut replies: Vec<&str> = Vec::new();
    for text in value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
    {
        if text.is_empty()
            || text.starts_with('/')
            || text.chars().count() > 48
            || text.chars().any(undisplayed)
            || replies.iter().any(|reply| reply.eq_ignore_ascii_case(text))
        {
            continue;
        }
        replies.push(text);
        if replies.len() == 3 {
            break;
        }
    }
    replies
}

fn undisplayed(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061c}'
                | '\u{200b}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}
