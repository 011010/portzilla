# Claude Code file intents — implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make the Claude Code adapter guard `Edit` and `Write` with the same file-intent leases OpenCode already uses, and surface clean-merge warnings through `PostToolUse.additionalContext`.

**Architecture:** `src/claude_code.rs` gains `hook_event_name` and edit-input parsing, and dispatches: `PreToolUse` + `Bash` keeps the kill guard; `PreToolUse` + `Write`/`Edit` claims with a location and denies only proven conflicts; `PostToolUse` + `Write`/`Edit` re-checks the written file and emits `additionalContext`. `portzilla init claude-code` prints two hook entries.

**Tech Stack:** Rust 2024, `std`, existing `intent`/`conflict` modules, Claude Code 2.1.283 hook contract.

**Design doc:** `docs/plans/2026-11-18-claude-code-file-intents-design.md`

---

## Global constraints

- **Silence means allow.** The clean path emits no JSON; never return `permissionDecision: "allow"`, which would skip the user's permission prompt.
- **`PostToolUse` never denies.** It may only add context.
- **Fork guard.** Any new git spawn (the conflict check already handles this) must keep the `cfg(test)` fork lock.
- **No new dependencies.**
- Conventional commits, no AI attribution, never stage `.gitignore` or `docs/CLI.md`.

---

### Task 1: Parse the payload and dispatch on the event

**Files:** `src/claude_code.rs`; tests in its `mod tests`.

**Step 1: failing tests**

```rust
#[test]
fn a_write_payload_exposes_the_file_and_content() { /* deserialize a PreToolUse payload; assert tool/file/content */ }

#[test]
fn an_edit_payload_exposes_old_and_new() { /* file + old_string + new_string */ }

#[test]
fn a_post_tool_use_payload_is_recognized_as_such() { /* hook_event_name == "PostToolUse" */ }

#[test]
fn the_bash_path_is_unchanged() { /* existing kill-guard assertions still pass untouched */ }
```

**Step 2:** run `cargo test -p portzilla claude_code` and confirm the new tests fail to compile.

**Step 3: implement**

- `PreToolUseInput` (rename if helpful) gains `hook_event_name: Option<String>`, `cwd: Option<String>`.
- `ToolInput` gains `file_path: Option<String>`, `content: Option<String>`, `old_string: Option<String>`, `new_string: Option<String>`.
- Keep `command` exactly as today.
- No verdict logic in this task: only parsing plus a `HookEvent` enum (`PreToolUse`, `PostToolUse`, `Other`).

**Step 4:** tests green; commit `feat(claude-code): parse edit payloads and the hook event`.

---

### Task 2: `PreToolUse` for `Write`/`Edit`

**Files:** `src/claude_code.rs`, `src/main.rs` (wiring).

**Step 1: failing tests**

```rust
#[test]
fn an_edit_of_a_free_file_is_claimed_and_allowed_silently() { /* no stdout; store has the claim with a location */ }

#[test]
fn a_conflicting_edit_on_a_held_file_is_denied_with_the_reason() { /* deny JSON, no claim takeover */ }

#[test]
fn a_clean_edit_on_a_held_file_is_allowed_silently() { /* no stdout; holder unchanged */ }

#[test]
fn a_held_file_without_a_location_is_denied() { /* v1 record */ }

#[test]
fn a_write_payload_uses_content_as_the_post_edit_version() { /* conflict and clean cases */ }
```

**Step 2:** run and confirm failure.

**Step 3: implement**

- `run_hook_claude_code` in `src/main.rs` opens the `IntentStore` next to the lease store and passes both to the adapter.
- Dispatch: `PreToolUse` + `tool_name` `Write`/`Edit` → new `handle_edit` path; `PreToolUse` + `Bash` → existing path; `PostToolUse` → Task 3; anything else → allow silently.
- `handle_edit`:
  1. Require `session_id`, `file_path`, and (for `Edit`) `old_string` + `new_string`, or (for `Write`) `content`. Missing → `Unprovable` → deny under the existing policy (deny, because a foreign claim exists only after the store call; a free file with a missing payload still claims — decide and document: claim first, then check, so the file is protected even when the payload is unusable).
  2. `locate_file(cwd, file_path)`; `claim(key, session, now, now + DEFAULT_TTL_SECS, Some(location))`.
  3. `Ok` → allow silently.
  4. `HeldBy { location: Some(holder), .. }` → `conflict::check(cwd, &holder, pending_edit)`:
     - `Clean` → allow silently (no claim takeover).
     - `Conflict` → deny: "<file> conflicts with the version session <holder> is editing right now…".
     - `Unprovable` → deny with today's wording.
  5. `HeldBy { location: None, .. }` → deny.
  6. Store error → existing fail-open / fail-closed policy.
- Reuse the reason wording from `src/opencode.rs` so the two harnesses read the same.

**Step 4:** tests green; commit `feat(claude-code): deny only conflicting edits on held files`.

---

### Task 3: `PostToolUse` warning

**Files:** `src/claude_code.rs`.

**Step 1: failing tests**

```rust
#[test]
fn a_clean_edit_on_a_held_file_emits_additional_context() { /* PostToolUse JSON with the holder named */ }

#[test]
fn an_edit_that_created_a_conflict_emits_a_conflict_context() { /* wording differs from the clean case */ }

#[test]
fn a_post_tool_use_without_a_foreign_claim_is_silent() { /* no stdout */ }

#[test]
fn post_tool_use_never_emits_a_permission_decision() { /* assert the JSON has no permissionDecision */ }
```

**Step 2:** confirm failure.

**Step 3: implement**

- New response shape: `{"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "…"}}`.
- Logic: if the file has a live foreign claim, read the caller's current file and the holder's, compare them through `conflict::check` with a `Write`-style whole-file comparison (the edit already happened; the caller's file is the post-edit version). Clean → clean-merge context. Conflict → conflict-created context.
- No foreign claim, own claim, missing payload, store error → silent (no output, exit 0).

**Step 4:** tests green; commit `feat(claude-code): warn the model about shared files after the edit`.

---

### Task 4: `init claude-code` prints both entries

**Files:** `src/claude_code.rs` (`SETTINGS_SNIPPET`), `src/main.rs` (init prose), `tests/cli.rs`.

**Step 1: failing assertion**

- `portzilla init claude-code` stdout contains a `PreToolUse` entry with matcher `Bash|Write|Edit` and a `PostToolUse` entry with matcher `Write|Edit`, both invoking `portzilla hook claude-code`.
- The prose (stderr) says an existing Bash-only registration must be updated, and that a clean edit is allowed without skipping the permission prompt.

**Step 2:** confirm failure.

**Step 3:** update the snippet and prose.

**Step 4:** tests green; commit `feat(claude-code): register the edit hooks in the init snippet`.

---

### Task 5: Documentation

**Files:** `README.md` (support matrix and file-intents limits), `docs/GUARD.md` (Claude Code section), `CHANGELOG.md`.

- State that Claude Code now covers file intents, with the two-event mechanism and the "clean means silent, conflict means deny" rule.
- Note the migration: existing installations need the new matcher.
- Update the "only OpenCode is covered today" sentence.

Commit `docs: document file intents on Claude Code`.

---

### Task 6: End-to-end with real Claude Code

**Files:** none.

**Step 1:** throwaway git repo with a **project-level** `.claude/settings.json` generated from `portzilla init claude-code` (never touch `~/.claude/settings.json`), and `portzilla` on PATH.

**Step 2:** session A edits line 1. Session B edits line 5 (disjoint): expect the edit to succeed and the model's tool result to carry the portzilla context line.

**Step 3:** session C edits line 1: expect a deny with the conflict reason and an unchanged file.

**Step 4:** `portzilla intent ls` shows the holder; `intent release` frees it; a later edit succeeds.

**Step 5:** clean up the temp repo and any intents written to an isolated `PORTZILLA_DATA_DIR`.

---

## Out of scope

- `MultiEdit` and other tools outside the verified contract.
- Migrating existing user settings.
- MCP tools for intents.
- Changes to the OpenCode adapter.
