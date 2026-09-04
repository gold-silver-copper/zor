use crate::osc::Flags;
use crate::rules::{
    schema::{Gate, Region, RuleSet, RuleState},
    view::ScreenView,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verdict {
    pub state: RuleState,
    pub visible: Flags,
    pub rule: Option<String>,
    pub region: Region,
}

pub fn evaluate(set: &RuleSet, view: &impl ScreenView) -> Verdict {
    let mut winner = None;
    for rule in &set.rules {
        let text = region_text(rule.region, set, view);
        if gate_matches(&rule.gate, &text)
            && winner
                .as_ref()
                .is_none_or(|(priority, _, _): &(i32, usize, String)| rule.priority > *priority)
        {
            winner = Some((
                rule.priority,
                winner.as_ref().map_or(0, |(_, index, _)| index + 1),
                rule.id.clone(),
            ));
        }
    }
    if let Some((_, _, id)) = winner
        && let Some(rule) = set.rules.iter().find(|rule| rule.id == id)
    {
        return Verdict {
            state: rule.state,
            visible: Flags {
                idle: rule.visible_idle,
                blocker: rule.visible_blocker,
                working: rule.visible_working,
            },
            rule: Some(id),
            region: rule.region,
        };
    }
    Verdict {
        state: RuleState::Idle,
        visible: Flags::default(),
        rule: None,
        region: Region::Whole,
    }
}

fn gate_matches(gate: &Gate, text: &str) -> bool {
    let lower = text.to_lowercase();
    gate.contains.iter().all(|needle| lower.contains(needle))
        && gate
            .regex
            .iter()
            .all(|pattern| regex::Regex::new(pattern).is_ok_and(|value| value.is_match(text)))
        && gate.line_regex.iter().all(|pattern| {
            regex::Regex::new(pattern)
                .is_ok_and(|value| text.lines().any(|line| value.is_match(line)))
        })
        && gate.all.iter().all(|child| gate_matches(child, text))
        && (gate.any.is_empty() || gate.any.iter().any(|child| gate_matches(child, text)))
        && gate.not.iter().all(|child| !gate_matches(child, text))
}

fn region_text(region: Region, set: &RuleSet, view: &impl ScreenView) -> String {
    let lines: Vec<_> = view.lines().map(|line| line.into_owned()).collect();
    match region {
        Region::Whole => view.text().to_owned(),
        Region::Title => view.title().to_owned(),
        Region::Progress => view.progress().map_or_else(String::new, |value| {
            format!("{}:{}", value.state, value.percent)
        }),
        Region::Bottom(n) => join(
            lines
                .get(lines.len().saturating_sub(n)..)
                .unwrap_or_default(),
        ),
        Region::Top(n) => join(lines.get(..n.min(lines.len())).unwrap_or_default()),
        Region::BottomNonEmpty(n) => lines
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, line)| !line.is_empty())
            .nth(n.saturating_sub(1))
            .map_or_else(String::new, |(index, _)| {
                join(lines.get(index..).unwrap_or_default())
            }),
        Region::TopNonEmpty(n) => lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.is_empty())
            .nth(n.saturating_sub(1))
            .map_or_else(String::new, |(index, _)| {
                join(lines.get(..=index).unwrap_or_default())
            }),
        Region::PromptBox => {
            let rules: Vec<_> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| horizontal_rule(line))
                .map(|(index, _)| index)
                .collect();
            if rules.len() < 2 {
                String::new()
            } else {
                let lower = rules.get(rules.len().saturating_sub(2)).copied();
                let upper = rules.last().copied();
                match (lower, upper) {
                    (Some(lower), Some(upper)) => join(
                        lines
                            .get(lower.saturating_add(1)..upper)
                            .unwrap_or_default(),
                    ),
                    _ => String::new(),
                }
            }
        }
        Region::AfterLastPromptMarker => marker_index(&lines, set.prompt_marker.as_deref())
            .map_or_else(String::new, |index| {
                join(lines.get(index.saturating_add(1)..).unwrap_or_default())
            }),
        Region::WholeUnlessAtPrompt => marker_index(&lines, set.prompt_marker.as_deref())
            .map_or_else(
                || view.text().to_owned(),
                |index| {
                    if lines
                        .get(index.saturating_add(1)..)
                        .unwrap_or_default()
                        .iter()
                        .any(|line| {
                            set.block_markers
                                .iter()
                                .any(|marker| line.starts_with(marker))
                        })
                    {
                        view.text().to_owned()
                    } else {
                        String::new()
                    }
                },
            ),
    }
}

fn join(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}
fn marker_index(lines: &[String], marker: Option<&str>) -> Option<usize> {
    let marker = marker?;
    lines.iter().rposition(|line| {
        line == marker
            || line
                .strip_prefix(marker)
                .is_some_and(|tail| tail.starts_with(' '))
    })
}
fn horizontal_rule(line: &str) -> bool {
    let value = line.trim();
    let count = value.chars().take_while(|ch| *ch == '─').count();
    count >= 3 || (count == 2 && value.chars().count() == 2)
}
