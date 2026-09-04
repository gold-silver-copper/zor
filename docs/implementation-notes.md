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

