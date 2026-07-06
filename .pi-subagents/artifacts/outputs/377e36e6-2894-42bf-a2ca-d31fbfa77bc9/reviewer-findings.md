## Review

- Correct: `completionItem/resolve` is advertised and implemented (`cwtools-rs/crates/lsp/src/config.rs:336`, `cwtools-rs/crates/lsp/src/main.rs:963`), and the focused completion tests passed.
- Blocker: cwtools-rs/crates/lsp/src/completion/mod.rs:493 — high — small context completions are marked `is_incomplete: false` based only on `ast.errors.is_empty()` — `did_change` replaces the live text while explicitly preserving the prior AST until debounced validation (`cwtools-rs/crates/lsp/src/main.rs:893-903`), and `ast_for` returns that prior AST whenever it exists (`cwtools-rs/crates/lsp/src/main.rs:521-526`). A completion request in the debounce window can therefore resolve against a clean but stale AST and be cached by the client as complete (`cwtools-rs/crates/lsp/src/completion/mod.rs:510-512`), so VS Code may stop re-querying and keep showing stale/out-of-context suggestions after the user edits into a new context.
- Note: No code edits were made.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Reported one concrete high-severity correctness finding at cwtools-rs/crates/lsp/src/completion/mod.rs:493 with supporting references to main.rs AST freshness behavior."
    }
  ],
  "changedFiles": [],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "git diff --stat main...HEAD && git diff --name-only main...HEAD",
      "result": "passed",
      "summary": "Listed 10 changed files in the branch diff."
    },
    {
      "command": "cargo test -p cwtools_lsp completion --no-default-features",
      "result": "failed",
      "summary": "Run from repo root failed because Cargo.toml is under cwtools-rs."
    },
    {
      "command": "cd cwtools-rs && cargo test -p cwtools_lsp completion --no-default-features",
      "result": "passed",
      "summary": "35 unit tests and 19 integration tests passed, 1 ignored."
    },
    {
      "command": "git diff --cached --name-only",
      "result": "passed",
      "summary": "No staged files."
    }
  ],
  "validationOutput": [
    "cwtools_lsp completion tests: 35 unit tests passed; lsp_tests completion filter: 19 passed, 1 ignored.",
    "Review finding is based on code inspection of completion/mod.rs and main.rs AST lifecycle."
  ],
  "residualRisks": [
    "Review focused on correctness bugs in the branch diff, not performance tuning or full workspace behavior."
  ],
  "noStagedFiles": true,
  "diffSummary": "Completion payload/responsiveness changes: deferred completion resolve data, filtering/capping, completion instrumentation, Arc<str> document text, and tests.",
  "reviewFindings": [
    "blocker: cwtools-rs/crates/lsp/src/completion/mod.rs:493 - clean-but-stale ASTs can produce `is_incomplete: false` completion lists after did_change, allowing clients to cache stale context suggestions."
  ],
  "manualNotes": "Findings were written to the requested artifact path."
}
```
