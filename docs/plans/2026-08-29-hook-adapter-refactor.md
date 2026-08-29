# Hook Adapter Refactor Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Centralize the shared kill-guard evaluation path used by harness adapters without changing any external hook contract.

**Architecture:** Add a private `hook_common` module that evaluates a normalized command/session request through `guard::check` and exposes only the resulting `Verdict` plus a small typed failure classification where needed. Adapters continue to parse their own payloads and render their own JSON, text, stderr, and exit-code responses. Migrate adapters incrementally and use the existing adapter suites as behavior locks.

**Tech Stack:** Rust 2024, existing `guard::Verdict`, `Lease`, `PidChecker`, Serde, Cargo test.

---

### Task 1: Create the shared evaluation module

**Files:**
- Create: `src/hook_common.rs`
- Modify: `src/main.rs`
- Test: `src/hook_common.rs`

**Step 1: Write shared behavior tests**

Add tests using the existing fake checkers and lease helpers for:

- allow for an unleased command;
- deny for a foreign live lease;
- allow for an own session lease;
- warn for an unresolvable process-name target.

The tests should pass a normalized request, not harness-specific JSON.

**Step 2: Run the new tests and verify the module is missing**

Run:

```bash
cargo test hook_common::tests
```

Expected: fail to compile because the module/function is not implemented.

**Step 3: Implement the minimal shared evaluator**

Define a private normalized request containing `command`, optional `session`, lease slice, and checker reference. Provide one function that calls `guard::check` with `self_pid: None` and the normalized session. Do not serialize responses or decide adapter exit codes here.

**Step 4: Register and run focused tests**

Run:

```bash
cargo test hook_common::tests
```

Expected: all shared evaluator tests pass.

### Task 2: Migrate JSON adapters

**Files:**
- Modify: `src/claude_code.rs`
- Modify: `src/codex.rs`
- Modify: `src/cursor.rs`
- Modify: `src/gemini.rs`

**Step 1: Replace direct guard calls**

For each adapter, preserve payload parsing and missing-field/fail-open/fail-closed handling. Replace only the direct `guard::check` invocation with the shared evaluator. Keep each adapter's existing `HookOutcome`, response structs, field names, and warning behavior unchanged.

**Step 2: Run adapter unit tests**

Run:

```bash
cargo test claude_code::tests codex::tests cursor::tests gemini::tests
```

Expected: all existing JSON adapter tests pass unchanged.

**Step 3: Inspect the diff for contract preservation**

Run:

```bash
git diff -- src/claude_code.rs src/codex.rs src/cursor.rs src/gemini.rs
```

Confirm that only evaluation wiring changed, not input/output schema or policy messages.

### Task 3: Migrate exit-code and shim adapters

**Files:**
- Modify: `src/kimi.rs`
- Modify: `src/windsurf.rs`
- Modify: `src/opencode.rs`

**Step 1: Replace direct guard calls**

Use the shared evaluator while retaining Kimi/Windsurf exit-code semantics and OpenCode's `{ action, reason }` protocol. Do not modify the OpenCode JavaScript plugin snippet.

**Step 2: Run adapter unit tests**

Run:

```bash
cargo test kimi::tests windsurf::tests opencode::tests
```

Expected: all existing tests pass unchanged.

**Step 3: Run integration contract tests**

Run:

```bash
cargo test --test cli hook_
cargo test --test guard_bypass
cargo test --test mcp_stdio
```

Expected: all hook and MCP wire-level tests pass, proving external behavior was preserved.

### Task 4: Remove duplicated imports and finalize documentation

**Files:**
- Modify: `src/claude_code.rs`
- Modify: `src/codex.rs`
- Modify: `src/cursor.rs`
- Modify: `src/gemini.rs`
- Modify: `src/kimi.rs`
- Modify: `src/windsurf.rs`
- Modify: `src/opencode.rs`
- Modify: `README.md`
- Modify: `docs/GUARD.md`

**Step 1: Clean up adapter internals**

Remove direct `guard` imports only where no longer needed and retain `Verdict` imports where response mapping still matches on it. Keep module documentation accurate: adapters translate wire contracts, while shared evaluation owns the common guard call.

**Step 2: Document the boundary**

Add a concise architecture note to the guard documentation explaining that adapters own wire compatibility and `hook_common` owns normalized evaluation. Do not document private implementation details as public API.

**Step 3: Run complete verification**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
git diff --check
```

Expected: all tests pass, including the full existing adapter and integration suites.

**Step 4: Review scope**

Run:

```bash
git status --short
```

Confirm the refactor does not alter release workflow, CI behavior, P0 lease semantics, or the watcher. Do not commit while the no-commit restriction remains active.
