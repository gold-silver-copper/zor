# zor

`zor` is a small PTY wrapper that observes terminal output and publishes agent state using OSC
7877. Its library can be used without default features to parse and format the wire protocol
without pulling in the wrapper runtime.

```sh
cargo run -- -- your-command
cargo run --no-default-features --lib
```

The 0.1 wrapper runtime currently provides byte-ordered PTY passthrough, terminal observation,
rule schema/evaluation primitives, hysteresis, title/event encoders, and nested-wrapper bypass.
See `DESIGN.md` for the full contract.

## Protocol-only use

```toml
zor = { version = "=0.1.0", default-features = false }
```

The protocol-only surface is `zor::osc::{AgentId, Flags, Report, State, format, parse}`.

## License

MIT
