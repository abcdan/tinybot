use crate::{App, audit, schedules, security::clip, store::Task};
use anyhow::Result;
use serde_json::json;

const PAGE_SIZE: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Overview {
    Tasks,
    Schedules,
    Reminders,
}

pub(super) async fn reply(app: &App, task: &Task, request: &str) -> Result<Option<String>> {
    let Some((kind, dutch)) = requested_list(request) else {
        return Ok(None);
    };
    let (heading, empty, lines) = if kind == Overview::Tasks {
        let rows = crate::store::task_views::open_overview(&app.db, task).await?;
        let lines: Vec<_> = rows
            .into_iter()
            .map(|(title, state)| format!("• {} · {}", one_line(&title), status(&state, dutch)))
            .collect();
        if dutch {
            (
                "Openstaande taken",
                "Je hebt geen openstaande taken.",
                lines,
            )
        } else {
            ("Open tasks", "You have no open tasks.", lines)
        }
    } else {
        let rows = schedules::list(&app.db, &task.user).await?;
        let lines = rows
            .iter()
            .filter(|row| {
                row.state == "active" && ((kind == Overview::Reminders) == (row.kind == "reminder"))
            })
            .filter_map(|row| {
                let zone = row.time_zone.parse().ok()?;
                let time = schedules::local_iso(row.next_run?, zone);
                let time = time.get(..16)?.replace('T', " ");
                Some(format!("• {} · {} ({})", one_line(&row.title), time, zone))
            })
            .take(PAGE_SIZE + 1)
            .collect();
        match (kind, dutch) {
            (Overview::Reminders, true) => (
                "Geplande herinneringen",
                "Je hebt geen geplande herinneringen.",
                lines,
            ),
            (Overview::Reminders, false) => (
                "Scheduled reminders",
                "You have no scheduled reminders.",
                lines,
            ),
            (_, true) => ("Geplande taken", "Je hebt geen geplande taken.", lines),
            (_, false) => ("Scheduled tasks", "You have no scheduled tasks.", lines),
        }
    };
    audit::record(
        "routing",
        "local_list_reply",
        "completed",
        json!({"kind": match kind { Overview::Tasks => "tasks", Overview::Schedules => "schedules", Overview::Reminders => "reminders" }}),
    )
    .await;

    if lines.is_empty() {
        return Ok(Some(empty.into()));
    }
    let first = lines
        .iter()
        .take(PAGE_SIZE)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let mut message = format!("{heading}\n\n{first}");
    if lines.len() > PAGE_SIZE {
        message.push_str(match (kind, dutch) {
            (Overview::Tasks, true) => "\n\nTik op Taken om de rest te bekijken.",
            (Overview::Tasks, false) => "\n\nOpen Tasks to see the rest.",
            (_, true) => "\n\nOpen Schedules om de rest te bekijken.",
            (_, false) => "\n\nOpen Schedules to see the rest.",
        });
    }
    Ok(Some(message))
}

fn requested_list(request: &str) -> Option<(Overview, bool)> {
    let request = request
        .trim()
        .trim_end_matches(['?', '.', '!'])
        .to_lowercase();
    let question = [
        "thanks, daarnaast, ",
        "bedankt, daarnaast, ",
        "dank je, daarnaast, ",
        "daarnaast, ",
        "thanks, also, ",
        "also, ",
    ]
    .iter()
    .find_map(|prefix| request.strip_prefix(prefix))
    .unwrap_or(&request);
    Some(match question.trim() {
        "which tasks are still open"
        | "what tasks are still open"
        | "what are my open tasks"
        | "list my open tasks"
        | "show my open tasks" => (Overview::Tasks, false),
        "welke taken staan er nog open"
        | "welke taken staan nog open"
        | "welke taken zijn er nog open"
        | "welke taken zijn nog open"
        | "toon mijn openstaande taken"
        | "wat zijn mijn openstaande taken" => (Overview::Tasks, true),
        "what tasks are scheduled"
        | "which tasks are still scheduled"
        | "list my scheduled tasks"
        | "show my upcoming schedules" => (Overview::Schedules, false),
        "welke taken staan er nog gepland"
        | "welke taken staan er gepland"
        | "welke taken zijn gepland"
        | "toon mijn geplande taken"
        | "welke geplande taken heb ik" => (Overview::Schedules, true),
        "list my reminders" | "what reminders are scheduled" | "show my scheduled reminders" => {
            (Overview::Reminders, false)
        }
        "welke herinneringen staan er nog gepland"
        | "welke herinneringen staan er gepland"
        | "toon mijn herinneringen" => (Overview::Reminders, true),
        _ => return None,
    })
}

fn one_line(title: &str) -> String {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = clip(&collapsed, 160);
    if title.contains(['`', '[', ']', '<', '>', '*', '_', '~', '\\']) || title.contains("://") {
        let longest = title.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
        let marker = "`".repeat(longest + 1);
        format!("{marker} {title} {marker}")
    } else {
        title.into()
    }
}

fn status(state: &str, dutch: bool) -> &'static str {
    match (state, dutch) {
        ("waiting" | "blocked" | "setup" | "uncertain", true) => "Heeft je aandacht nodig",
        ("waiting" | "blocked" | "setup" | "uncertain", false) => "Needs you",
        ("running", true) => "Bezig",
        ("running", false) => "Running",
        ("queued", true) => "In de wachtrij",
        ("queued", false) => "Queued",
        _ => "Open",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_local_read_only_for_an_explicit_list() {
        assert_eq!(
            requested_list("Thanks, daarnaast, welke taken staan er nog open?"),
            Some((Overview::Tasks, true))
        );
        assert_eq!(
            requested_list("Welke taken staan er nog gepland?"),
            Some((Overview::Schedules, true))
        );
        assert_eq!(
            requested_list("Show my scheduled reminders"),
            Some((Overview::Reminders, false))
        );
        assert_eq!(
            requested_list("Welke taken staan er nog open? Stuur ze naar Chris."),
            None
        );
        assert_eq!(requested_list("Is de taak van gisteren open?"), None);
    }
}
