# The Shape of zor

A small dedicated program that runs a shell or an agent in a pty, watches what the agent draws,
and announces the agent's state, **working**, **blocked**, **idle**, or **none**, in-band as an
escape sequence and out-of-band as event lines. It knows nothing about multiplexers. fux consumes
it; so does tmux, kitty, and plain koh on a phone.

- **Status:** proposal, audited against source (herdr 0.8.2, koh 0.11.0, vt100 0.16.2).
- **Date:** 3 Sep 2026
- **Name:** `zor`, from the Slavic root for sight (*vzor*, *zorkij*). Free on crates.io, 3 Sep 2026.
- **Relation to fux:** replaces the *Detection* section of `design.md`. fux spawns every pane
  through `zor` and reads its state OSC via koh's `take_unhandled_oscs()`. Detection code,
  rules, hysteresis, and fixtures leave fux entirely.

---

## Why a separate program

Detection is a pure function of one pane's byte stream plus its child process tree. It shares
nothing with layout, transport, or control. Kept inside fux it is useful only to fux users; as its
own binary it is useful the day it compiles:

- `zor -- claude` under tmux shows state in the window title through tmux's title passthrough.
- `koh connect` on Termux with `--on-bell` already notifies on the bell; with the wrapper the
  title carries the state glyph too, with no change to koh.
- A shell script can read the event line stream and do anything.

It also isolates the part of the system that changes most. Agents update their UIs every few
weeks; the rule set will churn. Releasing that churn on its own cadence keeps fux releases about
fux.

The cost is one extra process and one extra `vt100::Screen` per pane. vt100 is a few hundred
kilobytes of state at 200×50 and parses at memory speed; the process is a pty passthrough that
sleeps on two fds. Both are cheap enough that fux wraps every pane, not only agent panes, so
detection is a property of the pane and not of how the user typed the command.

---

## Surface

```sh
zor [options] [--] <command> [args…]    # run <command> in a pty; default: $SHELL -l
zor --events <path> …                   # also write event lines to a unix socket or fifo
zor --events - …                        # …or to fd 3 (stdout is the pty's)
zor --title never|prefix|replace …      # how to touch OSC 0/2 (default: prefix)
zor --no-osc …                          # never emit the state OSC (title only)
zor --rules <dir> …                     # extra rule files; later files win on the same agent
zor --agent <id> …                      # skip identification, force one rule set
zor --debug …                           # dump matched rules to stderr on each change
zor check <fixture.txt> [--agent id]    # evaluate one captured screen, print the verdict
zor agents                              # list the bundled rule sets and their versions
```

Everything not listed passes through untouched. The wrapper is transparent to the program inside:
same window size including pixel dimensions, same signals, same exit code, resize forwarded
from `SIGWINCH`. zor is not a terminal: it sets no `TERM`, answers no query, tracks no keyboard
protocol. Bytes from the
child reach stdout unchanged except for the OSC sequences the wrapper appends; bytes from stdin
reach the child unchanged.

---

## Architecture

```
  stdin ──▶ [pty master] ──▶ child (shell or agent)
                │
                └── child output ──┬──▶ stdout (byte-identical, plus state OSCs)
                                   │
                                   └──▶ vt100::Screen ──▶ regions ──▶ rules ──▶ raw verdict
                                                                                   │
                            /proc, sysctl ──▶ process tree ──▶ agent id ──▶ rule set
                                                                                   │
                                                                              hysteresis
                                                                                   │
                                                                  ┌────────────────┴──────────┐
                                                             state OSC + title           event lines
```

Four modules, each with one job and a test suite that does not need the others:

| Module | Job | Depends on |
|---|---|---|
| `pty` | spawn, passthrough, resize, exit status | portable-pty, libc |
| `screen` | one `vt100::Screen` plus title, bell count, OSC 9;4, per-drain change flag | vt100 |
| `rules` | region extraction over a `ScreenView`, rule evaluation, agent identification | regex, serde, toml |
| `state` | hysteresis machine, emitters | nothing |

koh's `terminal::ServerTerminal` already does what `screen` needs (callbacks for title, bell,
progress, unhandled OSC), but it is not on koh's stable surface and pulling koh pulls iroh. The
wrapper depends on vt100 directly and reimplements the eighty lines of callbacks. The
`ScreenView` trait is the same shape as koh's `predict::ScreenView` so a future shared crate is a
move, not a rewrite.

### Passthrough is the invariant

The wrapper's first job is to not be noticed. Output is written to stdout before it is parsed, in
the same chunks it arrived in; the parser runs on a copy after the write. Input is forwarded
byte-for-byte, including the terminal's responses to the child's queries (DA, DSR, XTGETTCAP),
which the wrapper never intercepts. The wrapper does not answer queries itself; the real terminal
does. If the child puts the terminal in raw mode or enables mouse reporting, the wrapper is not
involved: it put its own stdin in raw mode at start and passes everything.

The only bytes the wrapper adds are at chunk boundaries, never inside an escape sequence, because
vt100 tells the wrapper when the parser is between sequences. It emits at most one state OSC per
change, plus the modified title if `--title` is on. On exit it restores the terminal's title if it
touched it and prints nothing else.

### Identification: the process tree, not the command line

`zor -- claude` knows the agent. `zor` wrapping a shell does not, and must watch for one. Two
lookups, at different costs:

- **Foreground pgid, every tick, cheap.** The child shell's controlling-terminal foreground group:
  field `tpgid` of `/proc/<child>/stat` on Linux, `proc_pidinfo(PROC_PIDTBSDINFO).e_tpgid` on
  macOS. `tcgetpgrp` on the pty master is the Linux fallback when `tpgid` reads 0; it is not
  relied on elsewhere, being unspecified on a macOS ptmx.
- **Job listing, on demand, expensive.** The processes in that group: on Linux a breadth-first
  walk of `/proc/<pid>/task/*/children` from the child and from the group leader, keeping those
  whose pgrp matches, with `comm` and `/proc/<pid>/cmdline`; on macOS `proc_listpids` with
  `PROC_PGRP_ONLY`, `pbi_pgid` checked, and argv plus `argv0` from `KERN_PROCARGS2` (`argv0`
  reflects a runtime's `process.title`). A leader-only lookup is tried first and the full listing
  only if the leader is not an agent.

A process name is normalised before matching: `argv0` if present (macOS only; Linux has
`comm` and `cmdline`), else `comm`; if that is a generic runtime or shell (`sh`, `bash`, `zsh`,
`fish`, `node`, `bun`, `python`, `python3`; `tmux` is never an agent), scan argv for the wrapped
script, skipping flags and the values of flags that take one (`-r`, `--require`, `--loader`,
`--import`, `--experimental-loader`, `--inspect-port`, python's `-W -X`, and `-o -S -L`),
stopping at `--`, giving up on eval flags (`-e`, `--eval`, `-p`, `--print` for node and bun;
`-c` and `-m` for python; `-c` for shells); strip quotes, take the basename, strip `.js`,
`.mjs`, `.cjs`, `.py`; if nothing matches, canonicalise the path and try the target's basename.
The result is matched against each rule set's `process_names`. Among several matches in one job
the group leader wins; otherwise a score (3 when the normalised name differs from `comm`, that is
a wrapped script or a changed process title; 2 for a direct binary; 1 for a bare runtime),
earliest process on ties. A `ZOR_AGENT=<id>` variable in a process's
environment beats name matching, which is how a launcher script or an unknown fork can declare
itself; `--agent` sets the same thing for the command zor spawns.

Identification is polled, not evented, because there is no portable event for "the foreground
job changed". The tick reads only the pgid: every 500 ms while unidentified, 300 ms while
identified, 100 ms while an idle transition is pending. The job listing runs when the pgid
changes, every 5 s while identified (a process can `exec` in place), and inside an
**acquisition window**: for 8 s after the pgid changed while unidentified, or after the screen
changed with no agent, probe every 500 ms for the first 1.5 s and every 2 s after, since the next
agent launch is likely then.

Losing the agent has two cases. When the foreground job contains the pane shell again (the
child pid, with or without background jobs in its group), the agent exited: an
`idle` with an exit marker is emitted at once, and the next probe clears the agent, emitting
`none`. When the foreground job is something else and contains no agent (the agent shelled out,
or the listing failed), the agent is kept until six consecutive probes miss it, about thirty
seconds at the identified cadence; a subprocess does not flicker the indicator. A new agent in
the pgid, or the same agent relaunched, is a replacement: title and progress evidence are
cleared and the startup grace restarts.

### Rules: regions, gates, priorities

Each agent has a rule set: a TOML file with an id, aliases, `process_names`, an optional
`prompt_marker` with `block_markers`, and a list of rules. A rule names a target state, a
priority, a region, a gate, and up to three flags. The highest-priority rule whose gate matches wins; earlier in the file wins
a tie. If no rule matches while an agent is identified, the verdict is **idle**: the working
signals disappearing is how most agents show they have finished, and a rule set need not encode
"nothing is happening". A rule with `state = "skip"` matches and vetoes the update, which is how
a transcript viewer or a model picker (both look like nothing) is kept from flipping the state to
idle.

**The detection text.** Regions read a window, not the raw viewport. On the primary screen the
window is the last *rows* lines ending at the later of the last non-blank viewport row and the
cursor row, which reaches into scrollback when the bottom of the viewport is blank; trailing
blank lines are trimmed, each line is right-trimmed, wide-glyph continuation cells are skipped.
When the whole viewport is blank the window ends at the viewport bottom. On the alternate
screen it is simply the last *rows* rows. Lines are joined with `\n` and the text ends with one,
so `^`/`\A` anchors sit at line starts. The wrapper's `vt100::Parser` keeps a scrollback of at
least *rows* lines for this.

Regions are computed lazily from the `ScreenView` and memoised per evaluation:

| Region | Content |
|---|---|
| `title` | the OSC 0/2 title, control characters stripped, 256 chars |
| `progress` | the latest OSC 9;4 report as `<state>:<percent>` (`3:0`, `1:42`, `0:0` after a clear); empty until the first report |
| `whole` | the detection text |
| `bottom(n)` | the last *n* lines of it |
| `bottom_non_empty(n)` | from the *n*-th non-empty line counting up, through to the end, blank lines between included |
| `top_non_empty(n)` | from the start through the *n*-th non-empty line |
| `prompt_box` | the lines strictly between the second horizontal rule counting up from the bottom and the next rule below it; empty with fewer than two rules |
| `above_prompt_box` | everything before that upper rule; the whole text when there is no box |
| `last_line_above_prompt_box` | the last non-empty line of the above |
| `after_last_rule` | the lines after the last horizontal rule |
| `after_last_prompt_marker` | the lines after the last line that is the rule set's `prompt_marker` alone or starts with it followed by a space |
| `whole_unless_at_prompt` | the detection text, or empty while the agent is at its prompt: the last prompt-marker line has no line after it that starts with one of the rule set's `block_markers` |

A **horizontal rule** is a line whose trimmed text starts with a run of `─` (U+2500), and either
nothing follows the run or the run is at least three long. So `──` alone is a rule and `─── done`
is a rule. Only `─`; `---` in a transcript is text.

**Gates** are conjunctive and nest:

- `contains: [..]`, every substring present, case-insensitive against the lowercased region.
- `regex: [..]`, every pattern matches the region text (lines joined with `\n`).
- `line_regex: [..]`, every pattern matches at least one line.
- `all: [gate..]`, `any: [gate..]` (at least one), `not: [gate..]` (none).

A rule is a gate with metadata. Disjunction is spelled `any`; a negative guard is `not`. Guards
mostly sit on the idle rules: the Claude rule that reads a `❯` prompt box as idle carries `not`
gates for the blocked texts, so a permission prompt drawn above the box still wins, and the
blocked rule itself demands corroboration (a command preview plus a numbered `yes`/`no` line)
rather than the question alone, so a user typing "do you want to proceed?" into the box does
not trigger it. Load-time validation: every positive gate has at least one matcher, regexes
compile, ids are unique, and a set stays under 128 rules, 512 gates, 1024 matchers, 32 matchers
per gate, 512 characters per matcher, nesting depth 8.

**Flags.** `visible_idle = true` on an idle rule means the screen shows an unmistakable prompt;
that idle bypasses the hold below. `visible_blocker = true` on a blocked rule means a prompt the
user must answer is on screen; that blocked is re-announced periodically and outranks any state
the agent reports about itself. `visible_working = true` on a working rule means a spinner or
progress line is on screen rather than working being inferred. A flag change with the state
unchanged is published like a state change, so a consumer can tell "blocked, prompt visible"
from "blocked, inferred".

The region vocabulary, the gate algebra, Claude's priority ladder (title at 1100, skips at
1000, blocked at 980, working between 965 and 975, idle at 950, title-idle and progress-idle at
250; other agents use ladders of their own) and the flag idea come from studying herdr's
manifests. The schema, the region implementations
and every rule file are written fresh from captured panes. No herdr data is converted or
vendored. See *Reference code is studied, not copied* in `design.md`.

### Hysteresis

Raw verdicts are noisy: a spinner frame is a row that changes, a prompt redraw briefly looks idle.
The state machine between verdict and emission:

- **working → plain idle is held.** The first idle verdict starts a hold and switches the tick to
  100 ms; each further idle verdict counts a confirmation; the idle is published at three
  confirmations, four idle verdicts in a row, about 300 ms. Any other verdict cancels the hold. If
  the hold is still open 700 ms after it started, the idle is published anyway: the cap forces
  publication, it does not cancel.
- **Everything else publishes at once:** into working, into blocked, blocked → idle,
  working → idle when the rule carries `visible_idle`, and any change of a `visible_*` flag with
  the state unchanged. Waiting on a blocked prompt costs the user
  time; a false working costs nothing.
- **Startup grace:** for three seconds after an agent is first identified or replaced, idle
  verdicts are ignored. Agents draw their prompt before they start working on a queued argument.
  (herdr ignores every verdict in the window, blocked included; zor lets working and blocked
  through, since a permission prompt in the first three seconds is real.)
- **Skip** and an unidentified pane leave the state alone; the tick ends with the hold cleared.
  Rules are evaluated once per tick with the full title and progress; herdr's separate skip
  pre-check without them is not reproduced.
- **Agent loss** is handled by identification (above): idle with an exit marker, then none.
- **Blocked re-announce:** while a `visible_blocker` state persists it is re-sent on the event
  channel every 800 ms, so a notifier can nag. In-band it is sent once.
- **Heartbeat:** a stable state is repeated on the event channel every 800 ms with the same `seq`,
  so a consumer that attached late gets a value. This is zor's, not herdr's.
- **Idle scan skip:** while idle with no hold pending and no screen change since the last scan,
  the screen is not re-read.

The constants (100 ms, three confirmations, 700 ms cap, 800 ms refresh, 3 s grace, 500/300 ms
ticks, 5 s reprobe, 8 s acquisition, six misses) are herdr's, which spent months tuning them
against real agents. They are the one thing the wrapper takes from herdr as-is, as numbers,
since there is no other way to arrive at them than the same months.

---

## Output contracts

### In-band: the state OSC

```
ESC ] 7877 ; state=<working|blocked|idle|none> ; agent=<id> ; seq=<n> ST
```

- `7877` is not used by xterm, iTerm2, kitty, mintty, or ConEmu as far as their documentation
  shows; it sits above mintty's 7770 block and well away from iTerm2's 1337 and FinalTerm's 133.
  To be re-checked against each terminal's source before the first release.
- `state` is required. `agent` is present unless the state is `none`. `seq` increments per
  emission so a consumer that sees two payloads in one drain knows the order. Optional keys:
  `visible=idle|blocker|working` (comma-separated, the flags), `exited=1`, and `message=<text>`
  (URL-encoded, at most 128 bytes) for a self-reporting agent to say what it is waiting on.
- Terminated by `ST` (`ESC \`), never `BEL`, so a terminal that logs unknown OSCs does not ring.
- Emitted once per change, never periodically. A consumer that wants the current value on attach
  reads the title.

koh 0.11 receives it through `Callbacks::unhandled_osc` into `take_unhandled_oscs()` (a ring of
16 payloads, 256 bytes each; this payload is under 60). fux drains that ring on each pane drain
and updates the pane's agent state in the workspace. Terminals that do not understand OSC 7877
discard it, per ECMA-48. Confirming that on Terminal.app, iTerm2, kitty, alacritty, wezterm,
Termux and under tmux is the first manual test, before any rule is written.

### In-band: the title

With `--title prefix` (the default) the wrapper rewrites the child's OSC 0/2 to
`<glyph> <original title>` where the glyph is `●` working, `◐` blocked, `○` idle, and nothing for
none. `--title replace` emits `<glyph> <agent>` regardless of what the child set. `--title never`
passes titles through. The rewrite is the reason the wrapper parses OSC 0/2 rather than only
copying them: it must know the original to prefix it, and it must re-emit on a state change even
when the child did not.

### Out-of-band: event lines

One JSON object per line, on the path given by `--events` (unix socket, connected as a client; or
a fifo; or fd 3):

```json
{"t":"state","state":"blocked","previous":"working","agent":"claude","seq":12,"visible":["blocker"],"title":"◐ claude","ts":1756900000.123}
{"t":"state","state":"idle","previous":"blocked","agent":"claude","seq":13,"exited":true,"title":"claude","ts":1756900900.000}
{"t":"agent","agent":"claude","pid":48211,"ts":1756899990.001}
{"t":"agent","agent":null,"ts":1756901000.500}
{"t":"exit","code":0,"ts":1756901001.000}
```

The stable-state refresh every 800 ms goes here as a `state` line with the same `seq`, so a
consumer can distinguish a change (new `seq`) from a heartbeat; a persisting `visible_blocker`
is re-sent on the same cadence for notifiers that nag. `exited` marks the idle that an agent's
exit synthesises. `previous` and `title` are there because a notifier's two messages, "needs
attention" on entering blocked and "finished" on working or blocked becoming idle, need the
transition and a label, and should not have to keep state to get them. Writes are non-blocking and a
full pipe drops lines rather than stalling the pty; the next line carries the current state, so a
dropped line costs nothing durable.

This is what a notifier script, a status bar, or a test harness reads. It is not what fux reads;
fux is a terminal and takes the OSC.

---

## Rule sets and fixtures

Rule sets are bundled in the binary with `include_str!` and overridable by `--rules <dir>` and by
`$XDG_CONFIG_HOME/zor/rules/*.toml`. The first release ships rules for the agents the author can
capture panes for, Claude Code first; the rest follow as fixtures arrive. There is no remote
manifest update; a rule change is a release.

A fixture is a captured screen: `zor --debug` writes one on request (a keybinding, or
`SIGUSR1`) as a text file with the visible rows, the title, the progress state, and the expected
verdict in a header. `tests/fixtures/<agent>/<name>.txt`. The test suite evaluates every fixture
and asserts the verdict; `zor check` does the same for one file from the shell. A rule set is not
merged without a fixture for every state it can produce and for every guard it carries.

The hysteresis machine is tested separately with a scripted verdict sequence and a mock clock;
no pty is involved. The pty passthrough is tested with a scripted child that emits every escape
sequence class (CSI, OSC with BEL and ST terminators, DCS, split across chunk boundaries) and
asserts the bytes reaching stdout are identical apart from the wrapper's own OSCs.

---

## What fux does with it

- Spawns every pane as `zor --title never -- $SHELL` (or the configured default command). fux
  draws its own status, so it does not want the title touched.
- Reads OSC 7877 from `take_unhandled_oscs()` on each pane drain and sets the pane's agent state
  in `WorkspaceState`.
- Reads the `agent=` field to label the pane in the tab bar.
- Fires its notifier on transitions into **blocked** and **idle**, as before.
- Nothing else. fux carries no rules, no regex, no hysteresis, no process-tree code.

A user running plain koh on a phone runs `zor -- claude` on the host and gets the title glyph in
koh's status line and the bell hook as before.

---

## Risks

### Passthrough fidelity — *the whole product*

A wrapper that corrupts a query response or splits an escape sequence breaks the program inside it
in ways the user blames on the program. **Mitigation:** the wrapper never buffers output (write
first, parse a copy), never answers queries, and inserts its own bytes only when vt100 reports the
parser is in ground state. The passthrough test runs the vttest-style corpus through it.

### Double emulation cost — *bounded*

Two vt100 screens per pane under fux. **Mitigation:** measured with fux's chaos harness at 40
panes before v1. The screen cannot be shrunk below the pane's rows plus an equal scrollback, since
the detection window reaches into scrollback; a 200×50 vt100 screen is under a megabyte.

### Process-tree lookup on macOS — *reimplemented, not copied*

`proc_listpids` and `KERN_PROCARGS2` are unpleasant. herdr's `platform/macos.rs` shows what works;
the wrapper writes its own with the same syscalls. Failure degrades to identification by the
command line given to `zor`, so `zor -- claude` always works.

### The OSC number — *pick once*

If 7877 later collides with a terminal's own use, the consumer side is one constant in fux and
one in any script. The `state=` key/value form means a collision is detectable, not silent.
herdr's debug tooling recognises an agent-emitted `OSC 21337;status=working`, so at least one
agent already self-reports in-band. Before 7877 is final, find what emits 21337; if it is a real
agent, zor accepts 21337 as a self-report alongside 7877 and the contract documents both.

---

## Read from source

- `herdr/src/pane/agent_detection.rs:5-13`, `:36-77`, `:154-182` — the hysteresis constants and
  the hold: 100 ms recheck, 3 confirmations, 700 ms cap forces publication, 800 ms blocked
  refresh, 3 s startup grace; `visible_idle` bypasses the hold
- `herdr/src/pane.rs:276-284`, `:463-535`, `:2171-2196`, `:2372-2385`, `:998-1020` — tick rates
  500/300/100 ms, probe on pgid change or every 5 s, 8 s acquisition window at 500 ms then 2 s,
  grace window, six-miss confirmation for a foreign foreground job
- `herdr/src/pane.rs:330-414` — shell back in foreground: immediate idle with exit, then clear
- `herdr/src/detect/mod.rs:243-271`, `:317-373`, `:511-570`, `:685-720` — job member choice
  (leader, then 3/2/1 score), name normalisation, runtime list, symlink canonicalisation
- `herdr/src/detect/manifest.rs:140-198`, `:446-564`, `:1104-1127`, `:1237-1283`, `:1286-1530` —
  schema, evaluation and the idle fallback, region names, gate semantics (all conjunctive,
  `contains` lowercased), region extraction, `prompt_box` from the second rule up, the `─`-only
  rule test
- `herdr/src/pane/terminal.rs:2616-2624`, `:2756-2839`, `:3385-3399` — the detection window:
  rows ending at max(last non-blank, cursor), into scrollback; alt screen last rows
- `herdr/src/detect/manifests/claude.toml`, `codex.toml` — priority ladders, `line_regex`, `all`,
  `skip_state_update`, `visible_*`, `top_non_empty_lines(20)`
- `herdr/src/platform/linux.rs:203-262`, `:335-343`; `macos.rs:322-346`, `:362-386`, `:393-420` —
  children walk, `tpgid` from stat; `proc_listpids(PROC_PGRP_ONLY)`, `e_tpgid`, `KERN_PROCARGS2`
- `herdr/src/platform/mod.rs:346-354` — the `HERDR_AGENT` environment hint
- `herdr/src/detect/mod.rs:397-412`, `:571-585`, `:668-677` — per-runtime eval flags, value-taking
  flags, stripped suffixes
- `herdr/src/pane/agent_detection.rs:15-21` — `visible_working`, the third flag
- `herdr/src/pane/osc.rs:320-526`, `:608` — the OSC collector, evidence clearing, `OSC 21337`
- `herdr/src/app/actions.rs:23-55`, `:132-157`; `events.rs:69-87` — notifier transitions and
  payload: agent, previous state, title
- `herdr/src/pty/fd.rs:220-240` — `TIOCSWINSZ` with pixel sizes
- `herdr/src/detect/manifest/tests.rs:703-745` — inline screen tests asserting the rule id
- `koh/src/terminal/server.rs:9-20`, `:95-106`, `:210-217` — OSC 9;4 parsing, the unhandled OSC
  ring (16 × 256), `progress()` and `take_unhandled_oscs()`
- `koh/src/client/cli.rs:44-88` — `BellHook`, `KOH_TITLE` in the hook's environment
- `vt100-0.16.2/src/callbacks.rs:23`, `:66` — `set_window_title`, `unhandled_osc`

---

## Decisions

- One binary, one crate, pure Rust, MIT. Depends on vt100, portable-pty, regex, serde, toml,
  libc. Not on koh.
- Passthrough first: write before parse, never answer queries, insert only in ground state.
- Wrap the shell, identify by process tree with herdr's normalisation and scoring; `ZOR_AGENT`
  in a process environment and `--agent` short-circuit it.
- Gates are conjunctive, `contains` is case-insensitive, no match with an agent means idle,
  `prompt_box` is rule-delimited, the detection window reaches into scrollback.
- OSC 7877 with key/value payload, ST-terminated, once per change. Title prefix by default.
- JSON event lines out-of-band, non-blocking, with an 800 ms heartbeat.
- herdr's hysteresis and recheck constants are adopted as numbers, and its state machine as
  behaviour, with one deviation: the startup grace swallows idle only. Everything else is written
  fresh from captured panes; no herdr code or data enters the tree.
- Not adopted from herdr: remote rule updates and versions, socket-based hook integrations and
  per-agent install steps (the in-band OSC replaces them), handoff suppression, the resize replay
  workaround, terminal duties (`TERM`, XTGETTCAP, kitty keyboard state), the unused block-marker
  and `bottom(n)` region variants beyond what the table lists, Windows.
- OSC 133 shell-integration marks are not a signal in v1; the process tree already says when the
  shell is back. Revisit if an agent's exit is ever ambiguous.
- Rules bundled, overridable from a directory, shipped per release. No remote updates.
- A rule set needs a fixture per state and per guard before it merges.
- Linux and macOS, including Termux. Windows out of scope.

## Open questions

- **Should fux also accept the OSC from an unwrapped pane?** A future agent could emit OSC 7877
  itself. Yes, trivially, since fux reads the OSC and does not care who wrote it. Worth
  documenting the OSC as a contract agents may adopt. If zor sees the agent inside it emit OSC
  7877, a screen-visible blocker still outranks the agent's self-report; otherwise the
  self-report wins over the rules.
