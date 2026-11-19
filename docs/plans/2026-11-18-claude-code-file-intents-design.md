# Claude Code file intents — design

Date: 2026-11-18
Status: approved

## Problem

File intents ship for OpenCode only, because that harness hands the edit hook a session id the shim can pass along. Claude Code is the harness this repository supports best for the kill guard — it is the only one with end-to-end own-lease recognition — and its `PreToolUse` hook carries `session_id`, `tool_name`, `tool_input`, and `cwd`. There is no reason the same lease model cannot apply to `Edit` and `Write`.

## Verified contract (Claude Code 2.1.283, code.claude.com/docs/en/hooks)

- `Write` tool input: `file_path`, `content`.
- `Edit` tool input: `file_path`, `old_string`, `new_string`, `replace_all`.
- A `PreToolUse` matcher accepts `"Write|Edit"`.
- `PreToolUse` output supports only `permissionDecision` (`allow`/`deny`/`ask`/`defer`), `permissionDecisionReason`, and `updatedInput`. **There is no `additionalContext` on `PreToolUse`.**
- `PostToolUse` output supports `additionalContext`, which appends information to the tool result the model reads.
- Hook payloads carry `hook_event_name`, so one command can serve both events.

## Design

### One command, two events

`portzilla hook claude-code` dispatches on `hook_event_name`:

| Event | Tool | Behaviour |
|---|---|---|
| `PreToolUse` | `Bash` | Kill guard, unchanged |
| `PreToolUse` | `Write` / `Edit` | Claim with the file's location. Clean merge → **no output** (normal permission flow); proven conflict or unprovable → `deny` with the reason |
| `PostToolUse` | `Write` / `Edit` | Re-check the file that now exists and emit `additionalContext` for the model |

### Decision 1 — a clean merge stays silent, it does not auto-approve

`permissionDecision: "allow"` in `PreToolUse` **skips the user's permission prompt**. The existing Bash hook never returns `allow`; it returns nothing, so the normal flow decides. Allowing an edit is not the hook's job: the hook's job is to deny conflicts. The clean path therefore emits no JSON at all.

### Decision 2 — the warning is recomputed in `PostToolUse`, not stashed

The two hook invocations are separate processes and the payload carries no reliable call id, so a pending-warning map cannot be shared safely. Recomputing is stateless and stricter than the OpenCode version: the OpenCode shim simulates the pending edit, while Claude Code's `PostToolUse` compares the file **as written** against the holder's copy. That also lets it report a conflict the edit just created — something `PreToolUse` cannot know it caused.

Consequences:
- Clean after the edit → `additionalContext`: "<file> is also being edited by session <holder>; the versions merged cleanly."
- Conflict after the edit → `additionalContext`: "<file> now conflicts with session <holder>'s version; coordinate before the next edit." The edit already happened, so this informs rather than blocks.
- No foreign claim, or the claim is the session's own → no output.

### Configuration

`portzilla init claude-code` prints a `settings.json` snippet with two entries:

- `PreToolUse` with matcher `Bash|Write|Edit`
- `PostToolUse` with matcher `Write|Edit`

A user who registered the old snippet (Bash only) keeps the kill guard but gets no file protection until they update their settings. The repository never writes configuration, so there is no automatic migration; the docs and `init` output must say so.

### Failure handling

- Store errors and payload problems follow the existing claude-code policy: fail open by default, deny under `PORTZILLA_FAIL_CLOSED=1`.
- A file intent that cannot be proven clean (v1 record, missing edit payload, git failure) is denied, matching the OpenCode guard.
- `PostToolUse` never denies: its only failure mode is a missing warning, which is fail-open by construction.

### Relationship to the owner's claim

A clean edit does not take over the claim, exactly as in OpenCode: the holder keeps it, and the caller's next edit is checked again against the holder's evolving file.

## Testing

- Unit tests per payload shape: `Write` and `Edit` parsing, the deny/allow/context output shapes, `hook_event_name` dispatch, and the `Bash` path unchanged.
- `init claude-code` string assertions for both hook entries.
- Real end-to-end with Claude Code 2.1.283 in a throwaway project with a **project-level** `.claude/settings.json` (never the user's global file): session A edits line 1, session B edits a disjoint line (allowed plus visible context), session C edits line 1 (denied).

## Out of scope

- `MultiEdit` or any other editing tool not in the verified contract.
- Automatic migration of existing user settings.
- MCP tools for intents.
- Any change to the OpenCode adapter.
