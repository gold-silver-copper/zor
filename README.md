# zor

`zor` is a small PTY wrapper that observes terminal output and publishes agent state using OSC
7877. Its library can be used without default features to parse and format the wire protocol
without pulling in the wrapper runtime.

## Install

```sh
cargo install zor
```

## Usage

```text
zor [options] [--] <command> [args…]    # default: $SHELL -l
zor --events <path> …                   # unix socket or fifo event lines
zor --events - …                        # event lines on fd 3
zor --title never|prefix|replace …      # default: prefix
zor --no-osc …                          # title updates only
zor --rules <dir> …                     # later rule sets replace earlier ids
zor --agent <id> …                      # force one rule set
zor --debug …                           # diagnostics on stderr
zor check <fixture.txt> [--agent id]
zor agents
```

Everything else passes through untouched. Child output reaches stdout byte-for-byte before zor
appends its own OSCs; stdin, window size (including pixels), signals, and exit status propagate to
the child. zor does not set `TERM`, answer terminal queries, or implement keyboard protocols.

```sh
cargo run -- -- your-command
cargo run --no-default-features --lib
```

See `DESIGN.md` for the architecture and full protocol contract.

## Protocol-only use

```toml
zor = { version = "=0.1.0", default-features = false }
```

The protocol-only surface is `zor::osc::{AgentId, Flags, Report, State, format, parse}`.

## License

MIT
