# Observation contract, version 1

zor observes agent activity and emits presentation data. It does not provide transport admission,
remote access, workspace management, or proof of which process produced terminal bytes.

## OSC reports

The protocol-only library is `zor::osc`. `PROTOCOL_VERSION` is 1. Producers emit:

```text
ESC ] 7877;v=1;state=blocked;agent=example;seq=12;visible=blocker;exited=0 ESC \
```

`v`, `state`, and `seq` are required. Unknown versions, missing versions, duplicate known fields,
invalid enum values, invalid percent escapes, and invalid UTF-8 are rejected. Unknown extension
fields within version 1 are ignored. The state is `working`, `blocked`, `idle`, or `none`; an agent
identifier is required for non-`none` states and forbidden for `none`. Sequence numbers are u64
values scoped to one producer lifetime; consumers must not treat them as globally unique IDs.

Agent identifiers are limited to 64 ASCII letters, digits, dots, underscores, or hyphens. Optional
messages are at most 128 decoded UTF-8 bytes, percent-encoded on the wire. The complete encoded
report, including optional OSC framing and unknown fields, is limited to 1024 bytes. `parse`
accepts a complete BEL/ST-terminated OSC or the joined payload delivered by a terminal parser.
`format` emits ST termination. Terminal parsers also need bounded buffering before calling `parse`.

## JSON Lines

The optional event output uses the same `v: 1` schema marker. `t` selects `state`, `agent`, or `exit`;
records retain their documented timestamps, agent, sequence, visible flags, and lifecycle fields.
`ts` is Unix time in seconds (a floating-point value). State records contain `state`, `seq`, and
optional `previous`, `agent`, `pid`, `code`, `title`, `visible`, and `exited` fields. `visible` lists
currently detected `idle`, `blocker`, and/or `working` evidence; it can differ from the interpreted
state while a transition settles. `exited` denotes an observed agent lifecycle exit, not termination
of the wrapper. Agent records announce a detected agent/PID or `agent: null` when none remains.
Exit records contain the wrapped command's final code, using 128 plus signal number for signals.
Unknown fields may be ignored within version 1; consumers must reject unsupported versions.
Every record is one JSON object plus a newline and is limited to 2048 encoded bytes. Oversized
records are rejected. A stalled event sink drops bounded pending records rather than blocking the
wrapped terminal stream. Events may be lost; they are observations, not an authoritative audit log.

## Trust and consumers

Any pane process can forge OSC reports. Successful parsing establishes schema validity only, not
agent identity or authenticity. fux displays this as observed/self-reported state and owns no
agent-specific interpretation rules. Consumers must not authorize access based on these reports.

Without zor, panes continue normally. Invalid or unsupported reports are ignored by the consumer.
Observer/rule/sink failures must preserve byte forwarding, terminal queries, resize, signals,
exit status, and child cleanup. The wrapper's passthrough integration suite covers these contracts.


## Local multiplexer observer

`zor observe --socket PATH --pane ID --pid PID` observes a pane already owned by fux. Optional global `--rules DIR` and `--agent ID` arguments may precede `observe`. The observer does not spawn or signal the pane command and does not own its PTY.

The adapter reads bounded newline-delimited JSON `list` and `capture` responses through the local control socket and verifies the kernel peer UID. Replies have a 1 MiB cap and a two-second absolute read deadline. Captures request at most 128 KiB and are sampled every 100 ms. Pane identity is checked against both its ID and original PID on every sample; removal or replacement ends observation. Title, progress, and pane geometry are read as bounded data from the pane summary.

Detection and the existing state machine remain in zor. Reports use the OSC v1 schema above, one report per newline on stdout. A consumer must apply a report-size limit before parsing, reject malformed output, and keep observer backpressure independent of pane input/output. Closing or killing the observer must not terminate the observed command.

The fux control adapter negotiates control v1 by sending and verifying the eight-byte `FUXCTL1\n` preface before each RPC. A mismatch or stalled preface ends that sampling attempt without sending a command. Preface reads use an absolute two-second deadline. This is an independent wire consumer, not a fux library dependency.
