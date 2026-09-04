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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::load;
    #[allow(clippy::panic)]
    fn sets() -> Vec<RuleSet> {
        vec![
            load(
                Path::new("test.toml"),
                "id='claude'\nprompt_marker='>'\nblock_markers=[]\nrules=[]\n",
            )
            .unwrap_or_else(|error| panic!("{error}")),
        ]
    }
    fn process(pid: i32, comm: &str, argv: &[&str]) -> Process {
        Process {
            pid,
            ppid: 1,
            comm: comm.to_owned(),
            argv0: None,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            env_agent: None,
        }
    }
    #[test]
    fn wrapped_runtime_and_eval_flags_are_classified() {
        // Phase Z §2: runtimes resolve scripts while eval/module forms are rejected.
        let sets = sets();
        let job = Job {
            leader: 2,
            processes: vec![process(
                2,
                "node",
                &["node", "-r", "hook.js", "/x/claude.js"],
            )],
        };
        assert!(identify(&job, &sets).is_some());
        for argv in [
            vec!["node", "-e", "x"],
            vec!["python", "-m", "claude"],
            vec!["sh", "-c", "claude"],
        ] {
            let job = Job {
                leader: 2,
                processes: vec![process(2, argv.first().copied().unwrap_or_default(), &argv)],
            };
            assert!(identify(&job, &sets).is_none());
        }
    }
    #[test]
    fn environment_override_and_leader_win() {
        // Phase Z §2: ZOR_AGENT wins outright and matching leaders beat later processes.
        let sets = sets();
        let mut overridden = process(9, "unknown", &["unknown"]);
        overridden.env_agent = Some("claude".to_owned());
        assert_eq!(
            identify(
                &Job {
                    leader: 2,
                    processes: vec![overridden]
                },
                &sets
            )
            .map(|(_, pid)| pid),
            Some(9)
        );
        let job = Job {
            leader: 2,
            processes: vec![
                process(3, "claude", &["claude"]),
                process(2, "claude", &["claude"]),
            ],
        };
        assert_eq!(identify(&job, &sets).map(|(_, pid)| pid), Some(2));
        assert!(
            identify(
                &Job {
                    leader: 4,
                    processes: vec![process(4, "tmux", &["tmux", "claude"])]
                },
                &sets
            )
            .is_none()
        );
    }
}
