use regex::Regex;
use serde::Deserialize;
use std::{fmt, path::Path, str::FromStr};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Region {
    #[default]
    Whole,
    Bottom(usize),
    Top(usize),
    BottomNonEmpty(usize),
    TopNonEmpty(usize),
    PromptBox,
    AfterLastRule,
    AfterLastPromptMarker,
    WholeUnlessAtPrompt,
    Title,
    Progress,
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for Region {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let simple = match value {
            "whole" => Some(Self::Whole),
            "prompt_box" => Some(Self::PromptBox),
            "after_last_rule" => Some(Self::AfterLastRule),
            "after_last_prompt_marker" => Some(Self::AfterLastPromptMarker),
            "whole_unless_at_prompt" => Some(Self::WholeUnlessAtPrompt),
            "title" => Some(Self::Title),
            "progress" => Some(Self::Progress),
            _ => None,
        };
        if let Some(region) = simple {
            return Ok(region);
        }
        for (prefix, constructor) in [
            ("bottom(", Self::Bottom as fn(usize) -> Self),
            ("top(", Self::Top),
            ("bottom_non_empty(", Self::BottomNonEmpty),
            ("top_non_empty(", Self::TopNonEmpty),
        ] {
            if let Some(number) = value
                .strip_prefix(prefix)
                .and_then(|tail| tail.strip_suffix(')'))
                .and_then(|n| n.parse().ok())
            {
                return Ok(constructor(number));
            }
        }
        Err(format!("unknown region {value}"))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RuleState {
    Working,
    Blocked,
    Idle,
    Skip,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub regex: Vec<String>,
    #[serde(default)]
    pub line_regex: Vec<String>,
    #[serde(default)]
    pub all: Vec<Gate>,
    #[serde(default)]
    pub any: Vec<Gate>,
    #[serde(default)]
    pub not: Vec<Gate>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub state: RuleState,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub region: Region,
    #[serde(default)]
    pub visible_idle: bool,
    #[serde(default)]
    pub visible_blocker: bool,
    #[serde(default)]
    pub visible_working: bool,
    #[serde(flatten)]
    pub gate: Gate,
}

fn default_process_names() -> Vec<String> {
    Vec::new()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "default_process_names")]
    pub process_names: Vec<String>,
    pub prompt_marker: Option<String>,
    #[serde(default)]
    pub block_markers: Vec<String>,
    pub rules: Vec<Rule>,
}

#[derive(Debug)]
pub struct Error(String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}

pub fn load(path: &Path, source: &str) -> Result<RuleSet, Error> {
    let mut set: RuleSet =
        toml::from_str(source).map_err(|error| Error(format!("{}: {error}", path.display())))?;
    if set.process_names.is_empty() {
        set.process_names = std::iter::once(set.id.clone())
            .chain(set.aliases.clone())
            .collect();
    }
    if set.rules.len() > 128 {
        return Err(Error(format!("{}: more than 128 rules", path.display())));
    }
    let mut ids = std::collections::HashSet::new();
    let mut totals = (0usize, 0usize);
    for rule in &mut set.rules {
        if !ids.insert(rule.id.clone()) {
            return Err(rule_error(path, rule, "duplicate rule id"));
        }
        match rule.state {
            RuleState::Idle if rule.visible_blocker || rule.visible_working => {
                return Err(rule_error(
                    path,
                    rule,
                    "idle rule has mismatched visible flag",
                ));
            }
            RuleState::Blocked if rule.visible_idle || rule.visible_working => {
                return Err(rule_error(
                    path,
                    rule,
                    "blocked rule has mismatched visible flag",
                ));
            }
            RuleState::Working if rule.visible_idle || rule.visible_blocker => {
                return Err(rule_error(
                    path,
                    rule,
                    "working rule has mismatched visible flag",
                ));
            }
            RuleState::Skip
                if rule.visible_idle || rule.visible_blocker || rule.visible_working =>
            {
                return Err(rule_error(path, rule, "skip rule carries flags"));
            }
            _ => {}
        }
        validate_gate(path, &rule.id, &mut rule.gate, 0, &mut totals)?;
    }
    if totals.0 > 512 || totals.1 > 1024 {
        return Err(Error(format!(
            "{}: rule complexity limit exceeded",
            path.display()
        )));
    }
    Ok(set)
}

fn rule_error(path: &Path, rule: &Rule, problem: &str) -> Error {
    Error(format!("{}: rule {}: {problem}", path.display(), rule.id))
}

fn validate_gate(
    path: &Path,
    rule_id: &str,
    gate: &mut Gate,
    depth: usize,
    totals: &mut (usize, usize),
) -> Result<(), Error> {
    totals.0 += 1;
    let direct = gate.contains.len()
        + gate.regex.len()
        + gate.line_regex.len()
        + gate.all.len()
        + gate.any.len()
        + gate.not.len();
    totals.1 += gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if depth > 8 || direct > 32 {
        return Err(Error(format!(
            "{}: rule {rule_id}: gate complexity limit exceeded",
            path.display()
        )));
    }
    if direct == 0 {
        return Err(Error(format!(
            "{}: rule {rule_id}: gate has no matcher",
            path.display()
        )));
    }
    for value in gate.contains.iter_mut() {
        if value.len() > 512 {
            return Err(Error(format!(
                "{}: rule {rule_id}: matcher exceeds 512 bytes",
                path.display()
            )));
        }
        *value = value.to_lowercase();
    }
    for value in gate.regex.iter().chain(&gate.line_regex) {
        if value.len() > 512 || Regex::new(value).is_err() {
            return Err(Error(format!(
                "{}: rule {rule_id}: invalid regex",
                path.display()
            )));
        }
    }
    for child in gate
        .all
        .iter_mut()
        .chain(&mut gate.any)
        .chain(&mut gate.not)
    {
        validate_gate(path, rule_id, child, depth + 1, totals)?;
    }
    Ok(())
}
