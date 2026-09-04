use crate::{
    osc::AgentId,
    platform::{Job, Pid, Process},
    rules::RuleSet,
};
use std::path::Path;

#[must_use]
pub fn identify(job: &Job, sets: &[RuleSet]) -> Option<(AgentId, Pid)> {
    for process in &job.processes {
        if let Some(id) = process.env_agent.as_deref()
            && let Some(set) = sets.iter().find(|set| set.id == id)
        {
            return AgentId::new(set.id.clone())
                .ok()
                .map(|id| (id, process.pid));
        }
    }
    let mut best: Option<(bool, u8, usize, &RuleSet, Pid)> = None;
    for (index, process) in job.processes.iter().enumerate() {
        let Some((name, score)) = normalized(process) else {
            continue;
        };
        let canonical_name = std::fs::canonicalize(&name).ok().and_then(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        });
        let set = sets
            .iter()
            .find(|set| set.process_names.iter().any(|candidate| candidate == &name))
            .or_else(|| {
                canonical_name.as_ref().and_then(|name| {
                    sets.iter()
                        .find(|set| set.process_names.iter().any(|candidate| candidate == name))
                })
            });
        let Some(set) = set else { continue };
        let candidate = (
            process.pid == job.leader,
            score,
            usize::MAX - index,
            set,
            process.pid,
        );
        if best.as_ref().is_none_or(|value| {
            (candidate.0 && !value.0)
                || (candidate.0 == value.0 && (candidate.1, candidate.2) > (value.1, value.2))
        }) {
            best = Some(candidate);
        }
    }
    best.and_then(|(_, _, _, set, pid)| AgentId::new(set.id.clone()).ok().map(|id| (id, pid)))
}

fn normalized(process: &Process) -> Option<(String, u8)> {
    let original = process.argv0.as_deref().unwrap_or(&process.comm);
    let base = basename(original);
    if base == "tmux" {
        return None;
    }
    let runtimes = [
        "sh", "bash", "zsh", "fish", "node", "bun", "python", "python3",
    ];
    if !runtimes.contains(&base.as_str()) {
        return Some((base, if original != process.comm { 3 } else { 2 }));
    }
    let shell = matches!(base.as_str(), "sh" | "bash" | "zsh" | "fish");
    let python = matches!(base.as_str(), "python" | "python3");
    let mut skip_value = false;
    for argument in process.argv.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        if argument == "--" {
            continue;
        }
        if (shell && argument == "-c")
            || (python && matches!(argument.as_str(), "-c" | "-m"))
            || (!shell
                && !python
                && matches!(argument.as_str(), "-e" | "--eval" | "-p" | "--print"))
        {
            return None;
        }
        if takes_value(argument) {
            skip_value = !argument.contains('=');
            continue;
        }
        if argument.starts_with('-') {
            continue;
        }
        return Some((basename(argument), 3));
    }
    None
}

fn takes_value(value: &str) -> bool {
    [
        "-r",
        "--require",
        "--loader",
        "--import",
        "--experimental-loader",
        "--inspect-port",
        "-W",
        "-X",
        "-o",
        "-S",
        "-L",
    ]
    .iter()
    .any(|flag| {
        value == *flag
            || value
                .strip_prefix(flag)
                .is_some_and(|tail| tail.starts_with('='))
    })
}

fn basename(value: &str) -> String {
    let unquoted = value.trim_matches(['\'', '"']);
    let name = Path::new(unquoted)
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or(unquoted);
    [".js", ".mjs", ".cjs", ".py"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
        .to_owned()
}
