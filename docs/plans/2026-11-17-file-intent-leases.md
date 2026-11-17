# File-Intent Leases (OpenCode first) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Stop two parallel agent sessions from editing the same repo-relative file in different worktrees, by claiming the file before the edit lands, with OpenCode as the first harness.

**Architecture:** File intents live in their own locked store (`intents.json`), separate from the port lease store, so the existing `leases.json` format is untouched. The OpenCode shim intercepts edit tools, sends the target file and session to the existing `portzilla hook opencode` protocol, and the adapter denies edits to a file another live session holds. The guard core (`src/guard.rs`) is not changed in this slice.

**Tech Stack:** Rust 2024, `fs4` file locks, `serde_json`, `clap`, `assert_cmd` for CLI tests, the existing Node-based shim smoke test pattern in `src/opencode.rs` and `tests/cli.rs`.

---

## Decisions (confirm before Task 1)

1. **Separate store, not a new lease kind.** Lease keys are ports today (`store.get(port)`). Mixing paths into that model would force a `leases.json` format change and would break the existing guard. A new `intents.json` costs one more file and keeps the blast radius small.
2. **Key = (git common dir, repo-relative path).** Two worktrees editing `src/auth.rs` is the conflict case. The same absolute path in different worktrees is not. Outside a git repo, fall back to the absolute path.
3. **Auto-claim on edit, no agent cooperation required.** The first session to edit a file holds it for a TTL (default 30 min). Other sessions are denied while it is live. Explicit `portzilla intent claim` exists but is optional. This removes the "agents must remember to declare" weakness.
4. **Deny only, no warn in v0.** Same-file edits that would not actually merge-conflict will be blocked too. That is a known tradeoff; a `warn` mode is a follow-up, not v0.

## Task 0 result (see `2026-11-17-file-intent-leases-spike.md`)

- **The V1 shim does not load on OpenCode 2.3.0.** The plugin must be rewritten to the V2 shape (`export default { id, setup(ctx) }`). This is a prerequisite and also a bug in the current `portzilla init opencode`.
- Edit tools are `write` (`input.path`, `input.content`) and `edit` (`input.path`, `input.oldString`, `input.newString`). The shell tool is `shell`, not `bash`.
- Throwing in `execute.before` blocks the tool. Verified.
- `shell.create.before` has no session id. The design does not need one: auto-claim uses the `sessionID` from the edit hook.
- `apply_patch` or other multi-file tools were not observed. Still open.

---

### Task 0: Spike on the installed OpenCode (DONE)

**Files:**
- Created: `docs/plans/2026-11-17-file-intent-leases-spike.md` (findings)

Done: the V2 spy plugin loads, `write`/`edit` hooks fire with `sessionID`, throwing blocks the edit, and shell env injection works. Remaining open item: multi-file tools such as `apply_patch`.

**Commit** (findings doc only):
```bash
git add docs/plans/2026-11-17-file-intent-leases-spike.md docs/plans/2026-11-17-file-intent-leases.md
git commit -m "docs: record OpenCode V2 hook findings for file intents"
```

---

### Task 1: Intent record and locked store

**Files:**
- Create: `src/intent.rs`
- Modify: `src/main.rs` (add `mod intent;`)
- Test: unit tests inside `src/intent.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn claim_then_get_returns_the_same_intent() {
    let dir = tempfile::tempdir().unwrap();
    let store = IntentStore::open(Some(dir.path().to_path_buf())).unwrap();
    store.claim("repo:src/auth.rs", "session-a", 1_000, 1_800).unwrap();
    let got = store.get("repo:src/auth.rs").unwrap().unwrap();
    assert_eq!(got.session, "session-a");
    assert_eq!(got.expires_at, 1_800);
}
```

**Step 2: Run it to see it fail**

Run: `cargo test -p portzilla intent::tests::claim_then_get -- --nocapture`
Expected: FAIL, `IntentStore` not found.

**Step 3: Implement the minimum**

Define `Intent { key, session, created_at, expires_at }` with serde. `IntentStore` mirrors `Store` in `src/store.rs`: `open(override_dir)`, `lock_exclusive()` via `fs4`, a versioned envelope `{ "format_version": 1, "intents": [...] }`, and atomic write through a temp file plus rename. The file is `intents.json` in the same data dir resolved by `Store::resolve_data_dir`.

**Step 4: Run the test**

Expected: PASS.

**Step 5: Commit**

```bash
git add src/intent.rs src/main.rs
git commit -m "feat(intent): add locked file-intent store"
```

---

### Task 2: Claim conflict, same-session refresh, expiry

**Files:**
- Modify: `src/intent.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn other_session_cannot_claim_a_live_intent() {
    let store = temp_store();
    store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
    let err = store.claim("repo:a.rs", "session-b", 1_100, 1_900).unwrap_err();
    assert!(matches!(err, IntentError::HeldBy { ref session, .. } if session == "session-a"));
}

#[test]
fn same_session_refreshes_its_own_intent() {
    let store = temp_store();
    store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
    store.claim("repo:a.rs", "session-a", 1_500, 2_300).unwrap();
    assert_eq!(store.get("repo:a.rs").unwrap().unwrap().expires_at, 2_300);
}

#[test]
fn expired_intent_can_be_taken_by_another_session() {
    let store = temp_store();
    store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
    store.claim("repo:a.rs", "session-b", 1_801, 3_601).unwrap();
    assert_eq!(store.get("repo:a.rs").unwrap().unwrap().session, "session-b");
}
```

**Step 2: Run and confirm failure**

Run: `cargo test -p portzilla intent::tests`
Expected: FAIL on the three new tests.

**Step 3: Implement**

`claim(key, session, now, expires_at)` runs under the lock:
- No record, or expired record: write the new record.
- Same session: update `expires_at`.
- Different live session: return `IntentError::HeldBy { session, expires_at }`.

**Step 4: Run and confirm pass**

**Step 5: Commit**

```bash
git add src/intent.rs
git commit -m "feat(intent): enforce single live owner per file key"
```

---

### Task 3: Release and list

**Files:**
- Modify: `src/intent.rs`

**Step 1: Failing tests**

```rust
#[test]
fn release_only_by_owner() {
    let store = temp_store();
    store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
    assert!(store.release("repo:a.rs", "session-b").is_err());
    store.release("repo:a.rs", "session-a").unwrap();
    assert!(store.get("repo:a.rs").unwrap().is_none());
}

#[test]
fn list_drops_expired_entries_on_read() {
    let store = temp_store();
    store.claim("repo:a.rs", "session-a", 1_000, 1_800).unwrap();
    assert!(store.list(2_000).unwrap().is_empty());
}
```

**Step 2: Run, see failure**

**Step 3: Implement** `release(key, session)` and `list(now)`, both under the lock.

**Step 4: Run, see pass**

**Step 5: Commit**

```bash
git add src/intent.rs
git commit -m "feat(intent): add owner-only release and expiring list"
```

---

### Task 4: Normalize file keys per repository

**Files:**
- Modify: `src/intent.rs`

**Step 1: Failing test** (uses a real `git init` in a tempdir)

```rust
#[test]
fn same_relative_file_in_two_worktrees_shares_a_key() {
    let repo = git_repo_with_worktree(); // helper: main repo + linked worktree
    let k1 = key_for_file(&repo.main, Path::new("src/auth.rs"));
    let k2 = key_for_file(&repo.linked, Path::new("src/auth.rs"));
    assert_eq!(k1, k2);
}

#[test]
fn outside_git_falls_back_to_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let k = key_for_file(dir.path(), Path::new("notes.md"));
    assert!(k.starts_with('/') || k.contains(":/"));
}
```

**Step 2: Run, see failure**

**Step 3: Implement** `key_for_file(cwd, file)`:
- Run `git -C <cwd> rev-parse --git-common-dir` and `--show-toplevel`.
- Relativize `file` against the toplevel of the worktree that contains it.
- Key format: `<git-common-dir>:<relative-path>`.
- If git fails, key = absolute file path.

Use `std::process::Command`, not a git library, to match the repo's no-extra-deps style.

**Step 4: Run, see pass**

**Step 5: Commit**

```bash
git add src/intent.rs
git commit -m "feat(intent): key file intents by repo-relative path"
```

---

### Task 5: Adapter verdict for file edits

**Files:**
- Modify: `src/opencode.rs` (the `OpenCodeInput` struct and `handle_with_policy`)
- Test: inside `src/opencode.rs` `mod tests`

**Step 1: Failing tests**

```rust
#[test]
fn edit_of_file_held_by_other_session_is_denied() {
    // store seeded with session-a holding the key; input from session-b
    let outcome = handle_file_input(
        r#"{"session_id":"session-b","file":"/repo/src/auth.rs","cwd":"/repo"}"#,
        &seeded_store("session-a"),
    );
    let v = response_json(&outcome);
    assert_eq!(v["action"], "deny");
    assert!(v["reason"].as_str().unwrap().contains("session-a"));
}

#[test]
fn edit_by_holder_session_is_allowed_and_refreshes() { /* same, session-a -> allow */ }

#[test]
fn input_without_command_or_file_still_fails_open() { /* existing behavior */ }
```

**Step 2: Run, see failure**

**Step 3: Implement**
- Extend `OpenCodeInput` with optional `file` and `cwd`. Existing `command` path unchanged.
- If `file` is present and `command` is absent: compute the key, call `IntentStore::claim` with `now` and TTL 30 min.
  - Allowed: `{"action":"allow"}`.
  - `HeldBy`: `{"action":"deny","reason":"<file> is being edited by session <s> until <t>; ..."}`.
- Store errors fail open (or closed under `PORTZILLA_FAIL_CLOSED=1`), same as today.

**Step 4: Run all `opencode` tests**

Run: `cargo test -p portzilla opencode`
Expected: PASS, including the existing command-path tests.

**Step 5: Commit**

```bash
git add src/opencode.rs
git commit -m "feat(opencode): deny edits to files held by another session"
```

---

### Task 6: CLI `portzilla intent`

**Files:**
- Modify: `src/main.rs` (new `Intent` subcommand with `Claim`, `Ls`, `Release`, each with `--json`)
- Test: `tests/cli.rs` (new tests using `PORTZILLA_DATA_DIR`)

**Step 1: Failing CLI test**

```rust
#[test]
fn intent_claim_then_second_session_is_refused() {
    let data = tempfile::tempdir().unwrap();
    Command::cargo_bin("portzilla").unwrap()
        .env("PORTZILLA_DATA_DIR", data.path())
        .args(["intent", "claim", "src/auth.rs", "--session", "a"])
        .assert().success();
    Command::cargo_bin("portzilla").unwrap()
        .env("PORTZILLA_DATA_DIR", data.path())
        .args(["intent", "claim", "src/auth.rs", "--session", "b"])
        .assert().failure().code(1);
}
```

**Step 2: Run, see failure**

**Step 3: Implement** the three subcommands, reusing the key normalization from Task 4. `claim` uses `--session` or `PORTZILLA_SESSION`. Exit codes follow the existing convention (`0` ok, `1` error, `2` not found on `release`).

**Step 4: Run `cargo test --test cli`**, expect PASS.

**Step 5: Commit**

```bash
git add src/main.rs tests/cli.rs
git commit -m "feat(cli): add portzilla intent claim, ls, release"
```

---

### Task 7: Rewrite the shim to the V2 shape and cover edit tools

**Files:**
- Modify: `src/opencode.rs` (`PLUGIN_SNIPPET`, and the module doc that claims V1 `shell.env` gives own-lease recognition)
- Test: string assertions in `src/opencode.rs` tests plus the shim smoke test in `tests/cli.rs`

**Step 1: Failing tests**

- The snippet must `export default` an object with `id` and `setup`, and must not contain `tool.execute.before`.
- The snippet must hook `write` and `edit` through `ctx.tool.hook("execute.before")`, read `event.input.path`, and send `{ session_id: event.sessionID, file, cwd }`.
- The `shell` hook must map `event.tool === "shell"` to `input.command`, not `bash`.

**Step 2: Run, see failure**

**Step 3: Implement**
- Replace the V1 export with `export default { id: "portzilla", async setup(ctx) { ... } }`.
- Register `execute.before` for `write`, `edit`, and `shell`:
  - `shell`: keep the existing command check, sending `{ session_id, command }`.
  - `write`/`edit`: send `{ session_id, file: event.input.path, cwd }`. Resolve relative paths against the project directory.
- Deny throws `Error(reason)`, as before.
- Keep the 5000 ms timeout and fail-open behavior.
- Drop the `shell.env` injection of `PORTZILLA_SESSION` from v0. It is not available in V2 and the file-intent path does not need it.

**Step 3b: Fix the stale claim in the module doc**

The doc says OpenCode gives end-to-end own-lease recognition through `shell.env`. Update it to match the V2 findings.

**Step 4: Run tests**, expect PASS.

**Step 5: Commit**

```bash
git add src/opencode.rs tests/cli.rs
git commit -m "feat(opencode): guard edit tools with file intents"
```

---

### Task 8: Manual end-to-end check

**Files:**
- Modify: `docs/plans/2026-11-17-file-intent-leases-spike.md` (record results)

**Step 1:** Create two worktrees of one repo with `portzilla worktree`-style `git worktree add`.

**Step 2:** Start two OpenCode sessions, one per worktree, with the shim installed in `~/.config/opencode/plugin/portzilla.js`.

**Step 3:** Ask session A to edit `src/auth.rs` (or a test file of the same relative path). Then ask session B to edit the same relative path.

**Expected:** B's edit is rejected with a message naming session A. A's second edit succeeds. After TTL expiry, B's edit succeeds.

**Step 4:** Record the observed results, including any tool-name surprises, in the spike doc and commit.

---

### Task 9: Docs and changelog

**Files:**
- Modify: `README.md` (short "File intents" section, honest about deny-only and OpenCode-first)
- Modify: `docs/CLI.md` (`intent` commands)
- Modify: `CHANGELOG.md` (new `Unreleased` entry)
- Modify: `docs/ROADMAP.md` (new `v0.5` item, status Planned until merged)

**Step 1:** Write the sections. Keep them short and state the limits: deny-only, OpenCode-first, auto-claim by edit, no cross-machine coordination.

**Step 2:** `cargo test` (full suite) and `cargo clippy`, expect clean.

**Step 3: Commit**

```bash
git add README.md docs/CLI.md CHANGELOG.md docs/ROADMAP.md
git commit -m "docs: document file-intent leases"
```

---

## Out of scope for v0

- Claude Code adapter (next slice, reuses the same `intent` module).
- `warn` mode and conflict-aware allow (only block when `git merge-tree` shows a real conflict). This would reuse Clash's approach and is the first follow-up if deny-only is too strict.
- Multi-file tools if Task 0 shows them to be a gap (document, don't build).
- Any MCP tool for intents.
- Port/DB/cache slots (see the earlier discussion).

## Risks

- **Harness API drift.** OpenCode is moving to V2 plugins. The Task 0 spike is the gate; if V1 hooks stop firing, the work becomes a V2 port of the shim first.
- **False blocks.** Deny-only will block edits that would have merged cleanly. Measure this in Task 8 before deciding on `warn`.
- **Stale claims.** A crashed session holds its file until TTL. Acceptable for v0; surface the expiry in the deny message.
