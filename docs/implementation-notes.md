# Implementation notes

Phase 0 was checked against koh 0.11.0 (`7d1f514436b5cf24d250c2fb1f14bd4d195c155d`) and
vt100 0.16.2 on 2026-09-03.

- The always-built library consists only of `osc`; all wrapper/runtime code and dependencies are
  gated by the default `cli` feature.
- koh's OSC callback supplies parameters as byte slices. Consumers reconstruct the protocol input
  by joining those slices with `;`, so `parse` accepts both that payload and complete OSC frames.
- vt100 exposes callbacks for unknown OSC sequences but no parser ground-state query. Phase Z must
  therefore use its separately specified streaming ECMA-48 boundary tracker.
- Shared wire types live in `osc`. The CLI-only hysteresis state may import them, but `osc` must
  never depend on wrapper modules.
- The wrapper constructs vt100 with a fixed 65,535-line history budget. vt100 can resize the
  viewport but cannot grow its configured history after construction.
- Output has one owner in the main loop. The PTY reader sends chunks over a channel; the loop
  flushes each chunk before parsing it and before any future injected bytes.
- No genuine Claude Code panes were available. `rules/claude.toml.draft` is intentionally not
  registered as a bundle and `zor agents` reports that there are no bundled sets.
- The raw-mode restoration test is ignored by default because it deliberately changes fd 0 and
  requires an interactive controlling terminal; run it manually with
  `cargo test raw_guard_restores_terminal_attributes -- --ignored` from a disposable terminal.
- This implementation session ran on macOS without an installed Linux Rust target. Linux-specific
  `/proc` code therefore requires the Ubuntu CI job for native compilation and process-tree
  integration; only the target-independent scheduler/loss seams ran locally.
