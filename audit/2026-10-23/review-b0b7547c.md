# Concat bounded independent check

- Task: `TASK-FINAL-CHECK`
- Actor: `/root/final_check` (`independent_reviewer`), receiver: `/root`
- Repository: `E:/Concat`
- Base: `2535c2c3ed453e79b4339ecb438f0284dcf2adcf`
- Candidate: `b0b7547c1baecdfe3f94b21bbbcb6fc06671a2dc`
- Environment: Windows x64, Node `v24.21.0`
- Verdict for this candidate: **FAIL**, two concrete integration findings. Both were delivered to root for direct repair; this report does not certify a later revision.

## Scope

One bounded independent pass of the candidate diff: bridge transfer cancellation and terminal queue ordering; asset import/duplicate persistence and its generation/sync callers; asset search projection, Slint callback wiring, and literal locale calls. No repository-wide scan, additional reviewer, source modification, Cargo execution, branch operation, or external service request was performed by this reviewer.

## Findings

### CONCAT-REVIEW-001 — P2: a queued completion can report success after a closed tab's transfer is aborted

Locations: `extensions/concat-bridge/background.js:145`, `:163`, `:228`.

`onPageGone` aborts the active transfer and appends its failure step to the existing queue. If `done` was already queued, the aborted transfer's exception is swallowed, then that earlier `done` calls `endRun` with no error. Only cancellation overrides the terminal reason; disconnection does not. The subsequently queued close failure is skipped because the run is already ended. The result is a successful terminal request without the file ever being posted.

Executed an in-memory Node harness reusing the existing `background.test.mjs` setup and mock HTTP bridge, without changing repository files:

1. Submit `close-queued-done` and connect a Flow page.
2. Send `fileUrl` to a stalled fetch, then send `done`.
3. Wait for the fetch to start, then disconnect the page.
4. Assert that the terminal request is `fail`, never `done`.

`node --input-type=module -`, cwd `E:/Concat`: **exit 1**. Actual output:

```json
{"downloadAborted":true,"terminal":[{"path":"/v1/jobs/close-queued-done/done"}]}
```

Expected fix: persist the disconnect terminal reason so an already queued `done` becomes a close failure, and cover this ordering with a regression test. Root has assigned this correction to the original bridge author.

### CONCAT-REVIEW-002 — P1: caller cleanup still deletes the generation source preserved after an index failure

Locations in the reviewed candidate: `src/crates/concat/src/panes/generate.rs:741`, `:798`, `:1031`, and restart cleanup at `:274`.

The library's new copy/save/delete ordering keeps the original source when saving the index fails. Both generation completion paths nevertheless call `remove_dir_all(out_dir)` irrespective of the result, deleting that retained source. Drive sync similarly deletes its staging directory after `take_as` failure. If an immediate cleanup does not remove a remaining output, `GeneratePane::open` recursively removes the entire staging tree at the next launch. These existing callers prevent the candidate's library-level data preservation from holding across the complete workflow.

Evidence: the library regression log confirms index failure leaves its source available; direct tracing of the reviewed generation and sync callers shows the unconditional recursive removal immediately afterward. Root independently located the same integration issue and is replacing cleanup with empty-directory-only removal, with a failure/cleanup/reopen regression. This reviewer did not run that later regression or certify its result.

Expected fix: preserve unindexed outputs during completion, sync cleanup, and restart; document their recovery location. Data-bearing directories must not be removed recursively as routine cleanup.

## Evidence inspected

| Evidence | Observed result | Attribution |
| --- | --- | --- |
| Candidate diff and affected caller paths | PASS: inspected, two findings above | This reviewer; Git reads exit 0 |
| Queued-done plus disconnect reproduction | FAIL: erroneous `/done`, exit 1 | This reviewer, Node harness |
| `assets-red.log` | Expected FAIL, 14 passed / 4 failed on old production code | Root execution; log inspected |
| `assets-green.log` | PASS, 18 asset tests, zero failures | Root execution; log inspected |
| `concat-tests.log` | PASS, 176 library + 5 main tests | Root execution; log inspected |
| `bridge-tests.log` | PASS, 36 tests, zero failures | Root execution; log inspected |
| Workspace Clippy and final workspace tests | NOT_RUN by reviewer; root is executing them | No independent final exit result claimed |
| GUI interaction, live Chrome/Flow/ChatGPT, cloud accounts | NOT_RUN | Outside this check's runtime evidence |

## Limits and handoff

Asset duplicate validation deliberately checks metadata and recorded size; same-size content corruption is not detected. Copy-before-index adds a same-volume copy. These disclosed implementation limits are not extra speculative findings.

No additional defect was established in the asset search projection or locale-call change. Their actual rendered interaction remains unverified by this reviewer. Next owner: `/root`, to apply the already assigned fixes and verify the affected regressions, build/test gates, and authorized publication. Do not present this report as approval of a later candidate or as remote CI success.
