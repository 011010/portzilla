# Lease Identity and Port Occupancy Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make lease liveness PID-reuse-safe and prevent claims from selecting a port already occupied by an unregistered process.

**Architecture:** Extend the existing `PidChecker` abstraction to return a process start identity and store that identity optionally on each lease for backward-compatible JSON reads. Keep the file-locking store and pure guard logic unchanged except for the richer lease liveness check. Make `claim_in_place` probe the requested port before accepting it, while preserving the existing reassignment flag and adding a machine-readable reason.

**Tech Stack:** Rust 2024, serde/serde_json, sysinfo, fs4, clap, assert_cmd, tempfile.

---

### Task 1: Add process identity to the domain model

**Files:**
- Modify: `src/lease.rs:5-60`
- Test: `src/lease.rs:92-144`

**Step 1: Write the failing tests**

Add deterministic checker tests proving that a lease is alive only when the
recorded process identity matches the current process identity, while a legacy
lease with no recorded identity retains PID-only behavior.

**Step 2: Run the focused tests**

Run: `cargo test lease::tests -- --nocapture`

Expected: FAIL because `Lease` and `PidChecker` do not yet expose process-start
identity.

**Step 3: Implement the minimal domain change**

- Add `process_start_time: Option<u64>` to `Lease` with `#[serde(default)]`.
- Update `Lease::new` to accept or derive the identity without breaking test
  construction patterns unnecessarily.
- Change `PidChecker` to expose an identity lookup that can distinguish a
  missing process from a process with a start timestamp.
- Implement the lookup with `sysinfo::Process::start_time()` in
  `SystemPidChecker`.
- Make `Lease::is_alive` compare PID existence and the optional recorded start
  time; legacy leases with `None` use PID-only liveness.
- Update test fixtures with deterministic process identities.

**Step 4: Run the focused tests**

Run: `cargo test lease::tests -- --nocapture`

Expected: PASS.

### Task 2: Thread process identity through store claims and views

**Files:**
- Modify: `src/store.rs:83-190, 329-457`
- Modify: `src/view.rs:1-70`
- Modify: `src/main.rs:320-365`
- Modify: `src/mcp.rs:85-132`
- Test: `src/store.rs:954-1058`
- Test: `src/view.rs:60-149`

**Step 1: Write the failing tests**

Add tests proving that a new claim records the current identity for the target
PID when available, and that claim/view JSON includes the optional field without
changing existing fields or exit codes.

**Step 2: Run the focused tests**

Run: `cargo test store::tests view::tests -- --nocapture`

Expected: FAIL because claims do not capture process identity and views do not
expose it.

**Step 3: Implement the minimal store/view change**

- Capture the target PID identity while executing the locked claim operation.
- Preserve `Lease::new` call sites for tests and synthetic leases by allowing
  identity to be supplied separately where appropriate.
- Ensure replacement, idempotent reclaim, stale-lease reuse, and reassignment
  all write the new identity.
- Add the optional field to `LeaseView` and `ClaimView` with serde-compatible
  output.
- Keep old `leases.json` files readable.

**Step 4: Run the focused tests**

Run: `cargo test store::tests view::tests -- --nocapture`

Expected: PASS.

### Task 3: Detect occupied unregistered requested ports

**Files:**
- Modify: `src/store.rs:83-190, 329-429`
- Modify: `src/view.rs:38-70`
- Modify: `src/main.rs:1173-1195`
- Modify: `src/mcp.rs:100-132`
- Test: `src/store.rs:606-756`
- Test: `tests/cli.rs:16-63`

**Step 1: Write the failing tests**

Add a store test that binds the requested port without creating a lease and
asserts that `claim_in_place` chooses the next free port. Add a CLI test that
asserts the JSON result marks reassignment and reports the occupancy reason.

**Step 2: Run the focused tests**

Run: `cargo test store::tests::claim -- --nocapture`

Expected: FAIL because a claim with no existing lease currently accepts the
occupied requested port.

**Step 3: Implement the minimal claim change**

- Introduce a reason enum or stable string for reassignment, with values for a
  live lease conflict and an OS-occupied port.
- When the requested port has no live conflicting lease, probe the OS before
  accepting it.
- If occupied, scan forward from the requested port using the existing live
  lease and OS bind checks.
- Preserve current behavior for a free port, dead lease replacement, live lease
  conflict, and maximum-port overflow.
- Preserve `reassigned` for existing consumers and expose the reason in CLI/MCP
  JSON and human output.

**Step 4: Run the focused tests**

Run: `cargo test store::tests::claim -- --nocapture`

Expected: PASS.

### Task 4: Update documentation and compatibility coverage

**Files:**
- Modify: `README.md:149-159`
- Modify: `docs/CLI.md`
- Modify: `docs/PRD.md:35-42`
- Modify: `docs/ROADMAP.md:140-146`
- Test: `tests/validation.rs`

**Step 1: Write compatibility tests**

Add an integration test that writes a legacy lease JSON object without the new
field, then verifies `who`, `ls`, and `prune` still operate successfully.

**Step 2: Run the compatibility test**

Run: `cargo test --test validation legacy -- --nocapture`

Expected: PASS once the serde default behavior is in place.

**Step 3: Document the behavior**

- Explain PID plus process-start identity and the legacy fallback.
- Document that `claim` can reassign when the requested port is occupied even
  without a Portzilla lease.
- Document the reassignment reason in CLI/MCP JSON examples.
- State explicitly that the OS probe is advisory and does not reserve a port.

**Step 4: Run documentation-facing CLI tests**

Run: `cargo test --test cli -- --nocapture`

Expected: PASS.

### Task 5: Full verification

**Files:**
- No additional files expected.

**Step 1: Run formatting**

Run: `cargo fmt --all -- --check`

Expected: PASS with no formatting differences.

**Step 2: Run Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS with zero warnings.

**Step 3: Run the complete suite**

Run: `cargo test --all-targets`

Expected: PASS for all existing and new tests.

**Step 4: Inspect the diff**

Run: `git diff --check && git diff --stat`

Expected: no whitespace errors and only the planned domain, store, view, CLI,
MCP, test, and documentation changes.
