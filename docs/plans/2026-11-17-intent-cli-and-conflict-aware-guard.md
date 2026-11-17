# Intent CLI and conflict-aware guard — implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Give humans manual control over file intents (`claim`/`who`/`ls`/`release`) and make the OpenCode guard deny only genuinely conflicting edits, telling the model when a permitted edit is safe.

**Architecture:** The intent record gains an optional location (file, worktree, repo-relative path) so a peer can read the holder's version. A new `src/conflict.rs` proves cleanliness with `git merge-file` plumbing and returns `Clean`/`Conflict`/`Unprovable`, and only `Clean` changes the verdict (allow with a warn instead of deny). The CLI wires the existing store API to four subcommands. The shim sends the pending edit and appends the warn to the tool result via `tool.execute.after`.

**Tech Stack:** Rust 2024, `std`, `git` plumbing via `std::process::Command` (no new crates), `clap`, `assert_cmd`, OpenCode 2.3.0 V2 plugin API.

**Design doc:** `docs/plans/2026-11-17-intent-cli-and-conflict-aware-guard-design.md`

---

## Global constraints

- **Fork guard:** every new `std::process::Command` that runs `git` must take the test-only fork lock, or the suite regresses into the fd-inheritance race fixed in `aa189d4`:

  ```rust
  #[cfg(test)]
  let _fork_guard = crate::store::port_test_support::lock();
  ```

  Route all git spawns through one helper so this cannot be forgotten.
- **No new dependencies.**
- **Fail-safe, not fail-open, for the conflict check:** `Unprovable` denies. That is today's shipped behaviour, so a broken check can never make the guard weaker than it already is.
- Conventional commits, no AI attribution, no `Co-Authored-By`.
- Never stage `.gitignore`, `docs/CLI.md` when it holds unrelated local edits, `.atl/`, `.playwright-mcp/`, `graphify-out/`.

---

### Task 0: Spike — is `tool.execute.after` mutable in V2?

**Files:**
- Create (throwaway, outside the repo): `/private/var/folders/hl/ktc1dc_129537yl571qrgrh80000gn/T/opencode/pz-spike2/.opencode/plugin/spy.js`

**Step 1:** Install this plugin in a throwaway git repo and run one `opencode run` that edits a file:

```js
export default {
  id: "spy",
  async setup(ctx) {
    await ctx.tool.hook("execute.after", async (event) => {
      if (event.status === "completed" && typeof event.result?.output === "string") {
        event.result.output += "\n\nSPIKE-MARKER from the after hook";
      }
    });
  },
};
```

**Step 2:** Confirm the model's tool result actually contains `SPIKE-MARKER`.

- If **yes**: Task 5 appends the warn to `output.output` (or `result.output` — record the exact field name the hook received).
- If **no**: Task 5 falls back to `console.warn` and the docs must say the warning is log-only.

**Step 3:** Record the finding in `docs/plans/2026-11-17-intent-cli-and-conflict-aware-guard-design.md` under a new `## Spike result` heading, and commit.

```bash
git add docs/plans/2026-11-17-intent-cli-and-conflict-aware-guard-design.md
git commit -m "docs: record the V2 after-hook mutability spike"
```

---

### Task 1: Intent location and schema v2

**Files:**
- Modify: `src/intent.rs` (`Intent`, `StoredIntents`, `claim`, `key_for_file` neighbourhood)
- Test: `#[cfg(test)] mod tests` in `src/intent.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn claim_with_a_location_persists_it() {
    let store = temp_store();
    let location = IntentLocation {
        file: PathBuf::from("/repo/src/auth.rs"),
        worktree: PathBuf::from("/repo"),
        rel: PathBuf::from("src/auth.rs"),
    };
    store
        .claim("k", "session-a", 1_000, 1_800, Some(location.clone()))
        .unwrap();
    assert_eq!(store.get("k").unwrap().unwrap().location, Some(location));
}

#[test]
fn a_v1_file_without_locations_is_upgraded_on_write() {
    // Seed a v1 envelope (format_version 1, no location fields), claim, then
    // read the raw file and assert format_version == 2 and the record carries
    // the new location.
}

#[test]
fn an_unknown_format_version_is_rejected_without_rewriting() {
    // format_version 99: expect an error whose message names the version, and
    // byte-identical file contents afterwards.
}
```

**Step 2:** Run `cargo test -p portzilla intent::tests` and confirm the new tests fail to compile/assert.

**Step 3: Implement**

- `pub struct IntentLocation { pub file: PathBuf, pub worktree: PathBuf, pub rel: PathBuf }`, `PartialEq`, serde with `#[serde(default)]`.
- `Intent` gains `pub location: Option<IntentLocation>`, serialized as a single optional `location` object so a v1 record (missing key) deserializes to `None`.
- Bump `INTENT_FORMAT_VERSION` to 2; v1 files are read and upgraded on the next write; any version above 2 is rejected without rewriting (chosen message: `intents.json at <path> uses format_version <n>, which this build does not understand; upgrade portzilla. The file was not modified`).
- `claim` gains a fifth parameter `location: Option<IntentLocation>`; it stores the location on create and preserves the existing one on same-session refresh (a refresh must not erase it).
- New helper `pub fn locate_file(cwd: &Path, file: &Path) -> (String, IntentLocation)`: returns the existing key plus the parts, computing the repo-relative path once. `key_for_file` becomes a thin wrapper (`locate_file(...).0`) so its 36 tests keep passing unchanged.
- Update every existing `store.claim(...)` call in tests with `None`.

**Step 4:** `cargo test -p portzilla intent` → all green.

**Step 5: Commit**

```bash
git add src/intent.rs
git commit -m "feat(intent): record where a claim's working copy lives"
```

---

### Task 2: Conflict check

**Files:**
- Create: `src/conflict.rs`
- Modify: `src/main.rs` (`mod conflict;`)
- Test: `#[cfg(test)] mod tests` in `src/conflict.rs`

**Step 1: Write the failing tests** (real git, under the fork guard via the module's own helpers)

```rust
#[test]
fn disjoint_edits_are_clean() { /* base "a\nb\nc\n"; holder changed line 1; caller changes line 3 -> Clean */ }

#[test]
fn same_line_edits_conflict() { /* both change line 2 differently -> Conflict */ }

#[test]
fn a_file_new_on_both_sides_conflicts_unless_identical() { /* no base blob: different content -> Conflict; same -> Clean */ }

#[test]
fn a_missing_holder_file_is_unprovable() { /* holder path does not exist -> Unprovable */ }

#[test]
fn an_edit_whose_old_text_is_absent_is_unprovable() { /* Replace with an old string not in the file -> Unprovable */ }

#[test]
fn a_write_is_compared_as_the_whole_new_file() { /* PendingEdit::Write; clean and conflicting cases */ }
```

**Step 2:** `cargo test -p portzilla conflict` → fails because the module does not exist.

**Step 3: Implement**

```rust
pub enum PendingEdit<'a> {
    Replace { old: &'a str, new: &'a str },
    Write { content: &'a str },
}

pub enum ConflictVerdict {
    /// The three-way merge is clean; permitting the edit cannot produce a conflict.
    Clean,
    /// The merge has real conflicts.
    Conflict,
    /// The check could not be completed; callers must deny (today's behaviour).
    Unprovable,
}

pub fn check(
    caller_cwd: &Path,
    holder: &crate::intent::IntentLocation,
    edit: PendingEdit<'_>,
) -> ConflictVerdict
```

Algorithm:
1. `holder_version = fs::read_to_string(&holder.file)`; error → `Unprovable`.
2. `caller_current = fs::read_to_string(join(caller_cwd, &holder.rel))`; missing file reads as `""`.
3. Apply the edit: `Write` uses `content`; `Replace` uses `replacen(old, new, 1)` and is `Unprovable` when `old` is not present (the adapter's payload and the file disagree).
4. Base: `git -C caller_cwd merge-base HEAD` for the caller, and the same in `holder.worktree`, then `git -C caller_cwd show <merge_base>:<rel with forward slashes>`. A missing path in the base is "not in base", not an error.
5. No base blob: `holder_version == caller_version` → `Clean`, else `Conflict`.
6. With a base: write the three versions to uniquely named files under `std::env::temp_dir()` (pid + an atomic counter), run `git -C caller_cwd merge-file -p <holder> <base> <caller>`, delete the temp files whatever happens (a small `Drop` guard). Exit `0` → `Clean`, `>0` → `Conflict`, `<0` or spawn failure → `Unprovable`.
7. Every git spawn goes through one `git_command(cwd)` helper that applies the fork guard under `cfg(test)`.

Notes for the implementer:
- `git merge-file` exit codes: 0 = clean, positive = number of conflicts, negative/`>1` on error paths (127/129 when args are wrong).
- On Windows, arguments are passed as separate `OsStr`s, so paths with spaces need no quoting; the temp files themselves are the only paths written by us.
- Do not shell out to `sh`.

**Step 4:** `cargo test -p portzilla conflict` → green; `cargo clippy --all-targets --all-features -- -D warnings` clean.

**Step 5: Commit**

```bash
git add src/conflict.rs src/main.rs
git commit -m "feat(conflict): prove whether a pending edit really conflicts"
```

---

### Task 3: Adapter uses the verdict

**Files:**
- Modify: `src/opencode.rs` (`OpenCodeInput`, the file-intent branch of `handle_with_policy`)
- Test: `#[cfg(test)] mod tests` in `src/opencode.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn a_clean_edit_on_a_held_file_is_allowed_with_a_warning() {
    // Holder record with a location; holder file diverges on a different line
    // than the pending edit -> action "warn", reason mentions the holder, and
    // the store still names the original holder afterwards.
}

#[test]
fn a_conflicting_edit_on_a_held_file_is_denied() { /* overlapping edits -> "deny" + "conflict" in the reason */ }

#[test]
fn a_held_file_without_a_location_is_denied() { /* v1 record -> "deny", today's behaviour */ }

#[test]
fn a_missing_edit_payload_on_a_held_file_is_denied() { /* no old/new/content -> "deny" */ }

#[test]
fn a_warn_never_takes_over_the_claim() { /* holder + expiry byte-identical after a warn */ }
```

**Step 2:** run and confirm they fail.

**Step 3: Implement**

- `OpenCodeInput` gains `call_id: Option<String>` and `edit: Option<EditInput>`, where `EditInput { old: Option<String>, new: Option<String>, content: Option<String> }`.
- On `HeldBy`:
  - Build `PendingEdit`: `content` → `Write`; `old` + `new` → `Replace`; anything else → `Unprovable`.
  - `Clean` → `{"action":"warn","reason":"<file> is also being edited by session <holder>; your edit merges cleanly right now. Coordinate before touching the same lines."}`.
  - `Conflict` → deny, reason says the edit **conflicts** with the holder's version, names the file and the holder.
  - `Unprovable` → deny with today's wording.
  - Never claim on warn or deny.
- Update the module doc: deny is now conditional on a proven conflict.

**Step 4:** `cargo test -p portzilla opencode` → green.

**Step 5: Commit**

```bash
git add src/opencode.rs
git commit -m "feat(opencode): deny only edits that really conflict"
```

---

### Task 4: `portzilla intent` subcommands

**Files:**
- Modify: `src/main.rs` (new `Intent` command with a nested enum, printing, JSON views)
- Test: `tests/cli.rs`

**Step 1: Write the failing CLI tests**

```rust
#[test]
fn intent_claim_then_who_then_release_round_trips() { /* 0, holder named, then release succeeds and who exits 2 */ }

#[test]
fn intent_claim_by_another_session_exits_1_and_names_the_holder() { /* exit 1, stderr contains the holder session */ }

#[test]
fn intent_release_by_a_non_owner_exits_1() { /* exit 1 */ }

#[test]
fn intent_release_of_an_absent_claim_is_idempotent() { /* exit 0 */ }

#[test]
fn intent_claim_uses_portzilla_session_when_the_flag_is_absent() { /* env fallback */ }

#[test]
fn intent_claim_without_any_session_errors_clearly() { /* exit 1, message says how to supply a session */ }

#[test]
fn intent_ls_json_lists_only_live_claims() { /* one expired seeded record is absent from the JSON */ }

#[test]
fn intent_claim_honours_ttl_secs() { /* expires_at - created_at == the requested TTL */ }
```

**Step 2:** `cargo test -p portzilla --test cli intent_` → fails (unknown subcommand).

**Step 3: Implement**

- `Commands::Intent { #[command(subcommand)] command: IntentCommands }` with:

```rust
enum IntentCommands {
    /// Claim a file for this session so another session's edits are checked against it.
    Claim { path: PathBuf, #[arg(long)] ttl_secs: Option<u64>, #[arg(long)] session: Option<String>, #[arg(long)] json: bool },
    /// Show which session holds a file.
    Who { path: PathBuf, #[arg(long)] json: bool },
    /// List live file claims.
    Ls { #[arg(long)] json: bool },
    /// Release this session's claim before it expires.
    Release { path: PathBuf, #[arg(long)] session: Option<String>, #[arg(long)] json: bool },
}
```

- Session resolution: `--session`, else `PORTZILLA_SESSION`, else `CLAUDE_CODE_SESSION_ID` (reuse `resolve_cli_actor_session`); when absent, exit 1 with: `intent claim needs a session: pass --session or set PORTZILLA_SESSION`.
- Path → `locate_file(cwd, path)`; `claim` passes `Some(location)`, so CLI claims are conflict-checkable too.
- Default TTL: reuse `opencode`'s 30-minute constant (lift it into `intent.rs` as `pub const DEFAULT_TTL_SECS`), overridden by `--ttl-secs` (zero or above 24 h rejected with a clear message).
- Output: `ls` prints `KEY SESSION AGE EXPIRES`; `who` prints one line; `claim`/`release` print a confirmation. `--json` emits an array (ls) or an object with `key`, `file`, `session`, `created_at`, `expires_at`, `age_secs`, `live`.
- Exit codes: `EXIT_NOT_FOUND` (2) for `who`/`release` when no record exists; 1 for held-by-another and not-the-owner (the message names the holder); 0 otherwise.

**Step 4:** `cargo test -p portzilla --test cli intent_` → green.

**Step 5: Commit**

```bash
git add src/main.rs tests/cli.rs
git commit -m "feat(cli): add intent claim, who, ls and release"
```

---

### Task 5: Shim sends the edit and surfaces the warn

**Files:**
- Modify: `src/opencode.rs` (`PLUGIN_SNIPPET`, string assertions)
- Test: `src/opencode.rs` tests plus `tests/cli.rs` if the smoke test pins the payload

**Step 1: Failing assertions**

- The snippet sends `call_id` and, for `write`, `content: event.input?.content`; for `edit`, `edit: { old: event.input?.oldString, new: event.input?.newString }`.
- The snippet registers `tool.execute.after` and appends the stashed warn to the tool result.
- The snippet must **not** claim anything on warn (no store writes from the shim; it has none by design).

**Step 2:** run and confirm failure.

**Step 3: Implement**

- Add `call_id: event.callID ?? null` and the edit payload to the existing `askPortzilla` call.
- Keep a `pendingWarnings` map keyed by `callID`; in `execute.after`, append `"\n\nportzilla: " + reason` to the mutable result **if Task 0 proved mutability**; otherwise `console.warn` and say so in the snippet comment.
- Keep the 5 s timeout and fail-open behaviour.

**Step 4:** `cargo test -p portzilla opencode` → green.

**Step 5: Commit**

```bash
git add src/opencode.rs
git commit -m "feat(opencode): send the pending edit and surface clean-merge warnings"
```

---

### Task 6: Documentation

**Files:**
- Modify: `README.md` (File intents section: conflict-aware verdicts, warn, CLI)
- Modify: `docs/CLI.md` (the four `intent` commands, exit codes, JSON shapes)
- Modify: `CHANGELOG.md` (`Unreleased`)
- Modify: `docs/ROADMAP.md` (intent CLI + conflict-aware guard: Implemented)

**Step 1:** Write the sections. Say plainly that `Unprovable` denies, that the schema is now v2 and older binaries must be upgraded before sharing a data directory, and that the CLI claims carry locations so they are conflict-checkable.

**Step 2:** `cargo test` + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt --all -- --check`.

**Step 3: Commit**

```bash
git add README.md docs/CLI.md CHANGELOG.md docs/ROADMAP.md
git commit -m "docs: document the intent CLI and conflict-aware guard"
```

---

### Task 7: End-to-end with two real sessions

**Files:** none (verification only)

**Step 1:** Two linked worktrees of one repo, shim installed in both with `portzilla init opencode > .opencode/plugin/portzilla.js`.

**Step 2:** Session A edits line 1 of `src/auth.rs`. Session B edits **line 5** (disjoint). Expected: B is allowed and the model's tool result carries the portzilla warning naming A.

**Step 3:** Session C edits **line 1** (overlapping). Expected: denied, reason says conflict, file untouched.

**Step 4:** `portzilla intent ls` shows the holder; `portzilla intent release src/auth.rs --session <A>` frees it; a following edit by C is allowed.

**Step 5:** Clean up the test intents from the local data directory and remove the temp repos.

**Step 6:** Commit nothing (verification); report results in the PR body.

---

## Out of scope

- Audit history for intents.
- MCP tools for intents.
- A never-denying warn mode.
- Cross-machine coordination.
