# Intent CLI and conflict-aware guard — design

Date: 2026-11-17
Status: approved

## Problem

Two gaps in the file-intent feature shipped in #27:

1. **No manual control.** A claim lives until its 30-minute TTL. If a session ends, crashes, or simply finishes with a file, nothing can release it early: `portzilla init` gives agents the guard, but a human has no way to inspect or clear a claim. The deferred `intent` CLI subcommands are the missing half.

2. **Deny-only is too blunt.** Any edit to a file another live session holds is denied, including edits that would merge cleanly. Measured complaint from the plan that shipped #27: the feature blocks work that had no conflict to begin with.

## Slice 1 — `portzilla intent <claim|who|ls|release>`

| Command | Behaviour | Session required | Exit codes |
|---|---|---|---|
| `intent claim <path> [--ttl-secs N]` | Claims the path for the caller's session | yes | 0 created/refreshed, 1 held by another session (error names the holder) |
| `intent who <path>` | Shows the live holder | no | 0, 2 when there is no claim |
| `intent ls` | Lists live intents with age and expiry | no | 0 |
| `intent release <path>` | Releases the caller's own claim before the TTL | yes | 0 (idempotent when absent), 1 when not the owner |

- Session resolution: `--session`, falling back to `$PORTZILLA_SESSION`, using the same helper the lease commands use. A command that needs attribution errors when neither is present.
- Path resolution: `key_for_file(cwd, path)`, so relative paths resolve against the current directory and the same file in any worktree of a repo maps to one key.
- `--json` on every command, flat shapes mirroring the lease commands.
- No audit/history for intents in this slice: the flight recorder is scoped to leases. Recorded as follow-up, not silently dropped.

## Slice 2A — Conflict-aware deny

Today any foreign live claim denies. The guard will instead deny only when the pending edit would actually conflict with the holder's copy.

**Schema v2 for `intents.json`.** The record currently stores the repository key but not *where* the holder's working copy lives, so a peer cannot read the other version. Add `holder_file` (canonical absolute path at claim time). Compatibility follows the lease store's policy: v1 files are read and upgraded on the next write; unknown future versions are rejected without rewriting the file; older binaries cannot safely read v2 and must be upgraded before sharing a data directory.

**The check, when another live session holds the file:**

1. Holder's version: read from `holder_file`.
2. Caller's post-edit version: for `edit`, apply `oldString -> newString` in memory; for `write`, the payload's `content` is already the post-edit version.
3. Base version: `git merge-base` of the two HEADs plus `git show <commit>:<path>`. When the file exists in neither HEAD (new in both), treat it as a conflict unless the two versions are byte-identical.
4. Run the three versions through `git merge-file -p` (plumbing; no new dependency; same `std::process::Command` pattern `key_for_file` already uses). A non-zero exit means real conflicts.

**Verdict.** Conflicts: deny as today, with a reason that says the conflict is real. Clean: allow, keeping the holder's claim intact; the caller's next edit is checked again against the holder's evolving file, so a conflict that emerges later is still denied. Any failure inside the check (v1 record without `holder_file`, git missing, unreadable file) denies: that is exactly today's shipped behaviour, and the improvement only applies when cleanliness can be proven.

**Fork guard, carried from the #18/#27 root cause.** Every new `std::process::Command` git invocation must take the same test-only fork lock (`port_test_support::lock()` under `cfg(test)`) that `git_stdout` takes, or the fd-inheritance race returns to the suite.

## Slice 2C — Model-visible warn

When the check proves the edit clean, the caller is allowed but told why it is safe: the shim registers `tool.execute.after` and appends a line to the tool result the model reads ("edited X, which another session is also editing; clean merge for now").

Prerequisite spike: verify that `tool.execute.after` receives a **mutable** result in OpenCode 2.3.0. The earlier spike confirmed the hook fires but not that mutating `output` reaches the model. If it does not, the fallback is `console.warn` (log-only), which weakens the feature to "allow silently" and must be stated in the docs.

## Data flow

```
OpenCode edit (session B, file F)
  -> shim sends {session_id, file, cwd, edit:{old,new} | content, callID}
  -> portzilla hook opencode
       key = key_for_file(cwd, F)
       claim(key, B, now, now+ttl)
         - free/self        -> allow (silent)
         - held by A        -> conflict check
              conflict     -> deny (reason names A and the conflict)
              clean        -> allow + warn text (returned to the shim)
  -> shim: deny throws; warn is stashed by callID and appended in execute.after
```

## Error handling

- Store errors: unchanged fail-open / `PORTZILLA_FAIL_CLOSED` policy.
- Missing `holder_file` (v1 record) or any git failure during the check: deny (today's behaviour).
- Missing edit payload for a `write`/`edit` tool call: treat as "cannot prove clean" and deny, since the caller is already known to hold a foreign live claim.

## Testing

- Conflict-check unit tests with real git under the fork lock: disjoint edits (clean), same-line edits (conflict), file new in both (conflict; identical versions clean), missing base, unreadable holder file (deny).
- CLI tests per command: create, refresh, holder conflict (exit 1, names the holder), `who`/`release` not found (exit 2), release by non-owner (exit 1), `--json` shapes, `PORTZILLA_SESSION` fallback.
- Schema tests: v1 file upgraded on write; v2 rejected by an older reader is out of scope to simulate, but the version gate must reject unknown versions without touching the file.
- End-to-end with two real OpenCode sessions in two linked worktrees: a conflicting edit is denied; a clean edit is allowed and the model sees the warning.

## Out of scope

- Audit history for intents.
- MCP tools for intents (the hook covers agents; the CLI covers humans).
- A `warn` mode that never denies: proving cleanliness is the point, so an unprovable case denies.
- Cross-machine coordination: keys and claims are local to one data directory.
