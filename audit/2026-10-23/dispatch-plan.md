# Concat improvement dispatch — 2026-10-23

- User authorization: fetch and merge upstream, use agent-team-workflow and Odin IDE, implement improvements and fixes, validate, commit and push to the existing fork.
- Starting revision: `6b1a4e8b`; merged upstream: `1d04d147`; merged baseline: `2535c2c3ed453e79b4339ecb438f0284dcf2adcf`.
- Existing untracked `.agents/` and `result00.md` belong to the user and are excluded from this delivery.
- Environment: Windows x64, Cargo workspace `src/`, pinned Rust 1.93.1, Slint 1.18.1; reuse installed native dependencies under `E:/concat-deps`.

| Task / owner | Write scope | Acceptance | Dependencies |
| --- | --- | --- | --- |
| Integration / root | Main worktree; dispatch and delivery evidence, integration commits | Upstream is an ancestor; no unrelated user files staged; final remote SHA equals local main | All tasks |
| Bridge / integration agent | Separate worktree `E:/Concat-worktrees/bridge-20261023`; `extensions/concat-bridge/**` | Reproduce a concrete defect, add regression coverage, fix it, pass Node tests and syntax checks | Merged baseline |
| Assets / Rust agent | Separate worktree `E:/Concat-worktrees/assets-20261023`; `src/crates/concat-host/src/assets.rs` | Reproduce data preservation failures, fix minimal paths, pass affected host tests | Merged baseline; root schedules Cargo |
| Asset search / root | `concat/src/panes/generate.rs`, callback wiring, `ui/workspace/generate-page.slint`, locales and user documentation | Search names, prompts, models and providers without changing IDs/order; empty and unmatched states; app check and real launch | Merged baseline |
| Final independent check / separate agent | Read-only candidate diff | One evidence-focused review of the integrated changes; fix concrete findings without another review chain | Implementations and tests complete |

Odin is bound to the actual `E:/Concat` root with read-only permissions. Live project analysis and Rust LSP symbol lookup succeeded. It provides navigation and analysis; Cargo and Node produce the build/test evidence. No MCP permissions or provider configuration will be changed.

Required gates: workspace format, locales and model inventory, workspace Clippy/tests as required by CONTRIBUTING, affected regression tests, `cargo check -p concat`, native GUI build/startup, one final independent check, explicit `origin main` push and remote verification. GUI interaction depends on available desktop capture; any unavailable step must be reported as NOT_RUN. Live paid generation and account-bound cloud operations are outside the validation scope.

Current state: upstream merged without conflicts; implementation tasks in progress. No release or deployment is requested.
