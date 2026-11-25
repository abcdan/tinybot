#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Language {
    English,
    Dutch,
}

const DUTCH_TARGETS: &[&str] = &[
    "je laatste antwoord",
    "je vorige antwoord",
    "het laatste antwoord",
    "het vorige antwoord",
    "je antwoord",
    "dit antwoord",
    "dat antwoord",
    "dit",
    "dat",
    "deze",
    "het",
];
const ENGLISH_TARGETS: &[&str] = &[
    "your last answer",
    "your previous answer",
    "your last reply",
    "your previous reply",
    "your answer",
    "your reply",
    "this answer",
    "that answer",
    "this reply",
    "that reply",
    "this",
    "that",
    "it",
];

pub(super) fn request(text: &str) -> Option<Language> {
    if text.len() > 200 {
        return None;
    }
    let normalized = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let text = normalized.trim_end_matches(['?', '!', '.']).trim_end();
    let text = strip_words(text, &["please"]).unwrap_or(text);
    let text = [" alsjeblieft", " alstublieft", " please", " graag"]
        .iter()
        .find_map(|suffix| text.strip_suffix(suffix))
        .map_or(text, |rest| rest.trim_end_matches([',', ' ']));

    if dutch(text) {
        Some(Language::Dutch)
    } else if english(text) || matches!(text, "🙉" | "🙉\u{fe0f}") {
        Some(Language::English)
    } else {
        None
    }
}

fn dutch(text: &str) -> bool {
    if matches!(text, "als audio" | "als spraakbericht" | "als audiobericht") {
        return true;
    }
    let receiving = strip_words(text, &["mag ik", "kan ik", "zou ik"]);
    let instruction = receiving
        .or_else(|| {
            strip_words(
                text,
                &["kan je", "kun je", "kan jij", "kun jij", "wil je", "zou je"],
            )
        })
        .unwrap_or(text);
    let instruction = strip_words(instruction, &["ook", "even"]).unwrap_or(instruction);
    if receiving.is_none()
        && let Some((verb, target)) = instruction.split_once(' ')
        && let Some(tail) = after_phrase(target, DUTCH_TARGETS)
        && matches!(
            (verb, tail),
            ("lees", "voor" | "eens voor" | "even voor") | ("spreek", "in" | "eens in" | "even in")
        )
    {
        return true;
    }
    let instruction = if receiving.is_none() {
        strip_words(instruction, &["stuur", "geef"]).unwrap_or(instruction)
    } else {
        instruction
    };
    let Some(action) = after_phrase(instruction, DUTCH_TARGETS) else {
        return false;
    };
    let action = strip_words(action, &["ook even", "ook", "even", "kunnen"]).unwrap_or(action);
    if receiving.is_none()
        && matches!(
            action,
            "voorlezen" | "inspreken" | "hardop lezen" | "hardop voorlezen" | "voor me voorlezen"
        )
    {
        return true;
    }
    let Some(tail) = after_phrase(
        action,
        &[
            "als een spraakbericht",
            "als een audiobericht",
            "als een voice note",
            "als spraakbericht",
            "als audiobericht",
            "als voice note",
            "als audio",
        ],
    ) else {
        return false;
    };
    matches!(
        tail,
        "" | "krijgen" | "ontvangen" | "mogen" | "kunnen krijgen"
    ) || (receiving.is_none() && matches!(tail, "sturen" | "maken" | "kunnen sturen"))
}

fn english(text: &str) -> bool {
    if matches!(text, "as audio" | "as a voice note" | "as a voice message") {
        return true;
    }
    let receiving = strip_words(text, &["can i", "could i", "may i", "i'd like", "i’d like"]);
    let instruction = receiving
        .or_else(|| strip_words(text, &["can you", "could you", "would you", "will you"]))
        .unwrap_or(text);
    if receiving.is_none()
        && let Some(target) = strip_words(instruction, &["read out", "read back"])
        && ENGLISH_TARGETS.contains(&target)
    {
        return true;
    }
    if receiving.is_none()
        && let Some(target) = strip_words(instruction, &["read"])
        && let Some(tail) = after_phrase(target, ENGLISH_TARGETS)
        && matches!(tail, "aloud" | "out loud" | "to me")
    {
        return true;
    }
    let verbs: &[&str] = if receiving.is_some() {
        &["get", "have"]
    } else {
        &["send me", "send", "turn", "make"]
    };
    let instruction = strip_words(instruction, verbs).unwrap_or(instruction);
    let Some(action) = after_phrase(instruction, ENGLISH_TARGETS) else {
        return false;
    };
    matches!(
        action,
        "as audio"
            | "as a voice note"
            | "as a voice message"
            | "into audio"
            | "into a voice note"
            | "in audio"
    )
}

fn strip_words<'a>(text: &'a str, phrases: &[&str]) -> Option<&'a str> {
    phrases
        .iter()
        .find_map(|phrase| text.strip_prefix(phrase)?.strip_prefix(' '))
}

fn after_phrase<'a>(text: &'a str, phrases: &[&str]) -> Option<&'a str> {
    phrases.iter().find_map(|phrase| {
        let rest = text.strip_prefix(phrase)?;
        if rest.is_empty() {
            Some(rest)
        } else {
            rest.strip_prefix(' ')
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_requests_for_the_previous_answer_in_both_languages() {
        for text in [
            "Mag ik dat als audio?",
            "Kan ik dat ook even als audio krijgen?",
            "Kun je dit antwoord voorlezen?",
            "Zou je je vorige antwoord als spraakbericht kunnen sturen?",
            "Lees dit voor.",
            "Spreek dat in",
            "Als audio graag",
            "Stuur dit als een audiobericht, alsjeblieft!",
        ] {
            assert_eq!(request(text), Some(Language::Dutch), "{text}");
        }
        for text in [
            "Could I get that as audio?",
            "Please read out your last reply",
            "Can you read this aloud?",
            "Could you send me your last answer as a voice note, please?",
            "Turn that into audio",
            "as audio please",
            "🙉",
        ] {
            assert_eq!(request(text), Some(Language::English), "{text}");
        }
    }

    #[test]
    fn leaves_audio_topics_transcription_and_multi_action_prompts_for_chief() {
        for text in [
            "Wat is audio?",
            "Kun je dit audiobestand transcriberen?",
            "Mag ik dat als audio en stuur het naar Alex?",
            "Kan ik dat als audio sturen?",
            "Kan ik dat voorlezen?",
            "Kun je mijn PDF voorlezen?",
            "Can you read that PDF aloud?",
            "Where do I find the audio?",
            "Can I get that as audio? Also forward it.",
            "Can I send that as a voice note?",
            "Could I read that aloud?",
            "I can send that as audio",
            "Read my calendar out loud",
            "Not as audio please",
            "🙉 nice!",
        ] {
            assert_eq!(request(text), None, "{text}");
        }
    }
}
