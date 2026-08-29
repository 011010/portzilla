# Lease Watcher Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add an optional `portzilla watch` command that periodically prunes leases owned by processes that have exited and stops cleanly when interrupted.

**Architecture:** Put one watch cycle in a small testable function that opens the existing locked `Store`, calls `prune` with `SystemPidChecker`, and returns the removed leases or an error. The command runner owns the interval and shutdown signal, retries cycle errors after reporting them, and delegates output to a watch-specific renderer. The daemon never owns or replaces the CLI/MCP store protocol.

**Tech Stack:** Rust 2024, Clap, Tokio timers/signals, existing locked JSON `Store`, existing `LeaseView` serialization.

---

### Task 1: Add testable watch-cycle behavior

**Files:**
- Create: `src/watch.rs`
- Modify: `src/main.rs`
- Test: `src/watch.rs`

**Step 1: Write focused unit tests for one cycle**

Cover these cases using a temporary store and the existing fake `PidChecker` patterns:

- A cycle removes dead leases and returns them.
- A cycle leaves live leases untouched.
- A cycle with an unreadable/corrupt state returns an error instead of panicking.

Keep the cycle independent of sleeping or signal handling so tests finish immediately.

**Step 2: Run the focused tests and verify the new behavior is not implemented**

Run:

```bash
cargo test watch::tests
```

Expected: the new tests fail to compile or fail because the cycle function is not implemented yet.

**Step 3: Implement the cycle**

Add a function that opens the supplied data directory, calls `Store::prune(&SystemPidChecker)`, and returns the pruned leases. Keep errors intact with context. Expose only the minimum `pub(crate)` surface needed by `main.rs` and tests.

**Step 4: Register the module and run focused tests**

Run:

```bash
cargo test watch::tests
```

Expected: all watch-cycle tests pass.

### Task 2: Add the `watch` command and portable shutdown loop

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Modify: `src/watch.rs`

**Step 1: Add command-line arguments**

Add `watch` with:

- `--interval <SECONDS>` parsed as a positive duration, with a documented conservative default.
- `--json` for machine-readable cycle events.

Reject zero or invalid intervals before opening the store. Preserve the existing command behavior and output contracts.

**Step 2: Add the async runtime capability**

Enable Tokio's signal feature only if required by the implementation. Use Tokio's portable Ctrl-C signal handling rather than Unix-only signal APIs, so the command works on Windows as well as Unix.

**Step 3: Implement the loop**

Run one cycle immediately, then wait for either the configured interval or Ctrl-C. On a cycle error, report the error and continue waiting/retrying. On Ctrl-C, report a clean shutdown and exit successfully. A startup failure resolving/opening the data directory should return a normal command error.

Avoid detached background tasks. The process remains in the foreground and owns its lifecycle explicitly.

**Step 4: Run command help and focused tests**

Run:

```bash
cargo test watch::tests
cargo run -- watch --help
```

Expected: watch tests pass and help documents interval, JSON output, and foreground operation.

### Task 3: Define human and JSON cycle output

**Files:**
- Modify: `src/watch.rs`
- Modify: `src/main.rs`
- Test: `tests/cli.rs`

**Step 1: Add output contract tests**

Test the one-cycle/output path without depending on an infinite process:

- Human output identifies each pruned port, PID, and tag.
- JSON output emits a stable event object containing the event type and pruned lease views.
- An empty cycle is represented without pretending that leases were removed.

Use a test-only one-cycle entry point or extract rendering from the loop; do not sleep in integration tests.

**Step 2: Implement renderers**

Reuse `LeaseView` and existing sanitization/serialization conventions. Keep stdout machine-readable in JSON mode and send diagnostics/errors to stderr.

**Step 3: Run the focused CLI tests**

Run:

```bash
cargo test --test cli watch
```

Expected: all watch output tests pass without requiring a permanent process.

### Task 4: Update documentation and verify the complete change

**Files:**
- Modify: `README.md`
- Modify: `docs/CLI.md`
- Modify: `docs/ROADMAP.md`

**Step 1: Document usage and semantics**

Explain that `watch` is optional and foreground-only, prunes based on process liveness rather than lease age, retries transient store errors, and preserves leases when the recorded server process remains alive after an agent exits. Document the interval and JSON event behavior.

**Step 2: Run the full verification suite**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
git diff --check
```

Expected: all commands pass. Native Windows execution remains covered by the multiplatform CI matrix.

**Step 3: Review scope**

Run:

```bash
git status --short
```

Confirm that release workflow behavior and existing P0 changes are preserved. Do not commit while the no-commit restriction remains active.
