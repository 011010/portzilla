# CLI Reference

Complete command reference for `portzilla`. For a high-level overview, see [`README.md`](../README.md).

## Commands

Data-producing commands accept `--json` to print machine-readable output on stdout instead of human-readable text. Store-backed commands read and write the same file-locked JSON state file, so concurrent invocations from different processes are serialized safely.

### `portzilla claim <PORT> --tag <TAG> [--pid <PID>] [--session <SESSION>] [--json]`

Claims `<PORT>` for the given (or default) PID and tag.

- `--tag <TAG>` — required. Human-readable description of what the port is for.
- `--pid <PID>` — optional. Defaults to the PID of the live parent process (the shell or agent invoking `portzilla`); falls back to `portzilla`'s own PID if the parent cannot be determined. An explicit PID does not need to exist: a nonexistent PID, or one whose process identity cannot be resolved, is accepted and recorded as an unverified, dead lease. It does not promise future ownership.
- `--session <SESSION>` — optional. Groups related leases under a session identifier.
- `--json` — print the outcome as JSON instead of a one-line summary.

Conflict resolution:

- No live lease on the requested port: `portzilla` probes the OS too. If an unregistered process has bound it, the next port with neither a live lease nor an OS bind is claimed instead (`reassigned: true`, `reassignment_reason: "os_occupied"`). Otherwise the requested port is claimed directly.
- A live lease on the requested port already owned by the same PID: the lease is updated in place (idempotent re-claim — tag and session can change, the port does not). Not reassigned.
- A live lease on the requested port owned by a different PID: `portzilla` finds the next free port at or after `requested_port + 1` (skipping ports with a live lease and ports the OS reports as bound) and claims that instead (`reassigned: true`, `reassignment_reason: "lease_conflict"`).

OS occupancy probing attempts wildcard and loopback addresses in both families: `0.0.0.0`/`127.0.0.1` and `::`/`::1`. A port is considered OS-free only when all usable probes succeed, which covers listeners bound to any local IPv4 or IPv6 interface, including localhost. Some platforms allow a wildcard bind alongside a loopback-only listener, so both IPv4 addresses and both IPv6 addresses are probed. On a platform where IPv6 is unavailable or unsupported, the IPv6 probes are ignored and the IPv4 results are used; this avoids treating every port as occupied on IPv4-only systems. Other IPv6 bind failures are treated as occupancy.

### `portzilla ls [--json]`

Lists every recorded lease. Human output is a table (`PORT PID STATUS AGE TAG`); JSON output is an array of lease objects (see shape below).

### `portzilla who <PORT> [--json]`

Shows the lease recorded on `<PORT>`. Exits with code `2` and prints nothing to stdout if no lease exists on that port.

### `portzilla release <PORT> [--json]`

Removes the lease recorded on `<PORT>` and prints the removed lease. Exits with code `2` if no lease exists on that port. If the owning PID is still alive at the time of release, a warning is printed to stderr (`release` always wins — it does not check ownership or refuse to act).

### `portzilla prune [--json]`

Removes every lease whose owning PID is no longer alive and prints each one that was removed. Human output prints `no dead leases to prune` if nothing was pruned; JSON output prints `[]`.

### `portzilla run <PORT> --tag <TAG> [--session <SESSION>] -- <COMMAND...>`

Claims `<PORT>` and runs `<COMMAND...>` with the lease held by the child process while it runs, so `who` names the real server PID and `prune` reaps it when the child exits. Do not pre-claim from an ephemeral shell — `run` claims it and holds it for the server in one step.

- `<PORT>` — required, 1-65535 (port 0 rejected, same as `claim`).
- `--tag <TAG>` — required, same 1024-char cap as `claim`.
- `--session <SESSION>` — optional, same 512-char cap as `claim`.
- `-- <COMMAND...>` — required, everything after `--` is executed directly with no shell. For a pipe or compound command, wrap it as `-- sh -c '...'`.
- No `--json` mode: the child owns stdout, so `run` accepts no JSON flag. Progress and reassignment notes go to stderr; server stdout stays untouched.

Environment passed to the child:

- `PORTZILLA_PORT` — always set to the actual (possibly reassigned) port. The child command must consume this variable; the exact command is framework-specific.
- `PORTZILLA_SESSION` — set only when `--session` was supplied; otherwise removed from the child's environment.

Lease transfer guarantee: `run` claims the port for its own wrapper PID (requiring a verified process start time — it refuses to spawn without one), spawns the child with inherited stdio, then atomically transfers the live wrapper lease to the spawned child PID, preserving port, tag, and session. The transfer retries while the child is still alive (a just-spawned child is not always immediately visible to the PID checker).

Reassignment: same conflict semantics as `claim` — a live foreign lease or an OS-bound port reassigns to the next free port, and the child is told the actual port. Reassignment is reported on stderr (`port <requested> is busy; running on port <actual> instead`); the non-reassigned case prints `running on port <actual>` to stderr.

Exit-status propagation: `run` waits for the child and exits with its status — the same code on a normal exit, `1` when the child was signaled and there is no code to propagate. A child that already exited before the transfer ran already ran with the right environment, so `run` reaps it and propagates its status instead of failing.

Cleanup: `run` never explicitly releases the lease. The short-lived wrapper lease is left for `prune` to reap if anything fails before the transfer; after a successful transfer the lease follows the child, and the child's exit leaves a dead lease that `prune` (or `watch`) removes. A child that stays alive yet unresolvable past the transfer deadline is stopped and reaped, then reported as an error — never touching a lease not verified as ours.

### `portzilla watch [--interval <SECONDS>] [--json]`

Runs an optional foreground watcher that repeatedly performs the same
process-liveness-based cleanup as `prune`. It is not a central daemon or IPC
service: it uses the existing locked `leases.json`, and claims and queries
continue to use the normal CLI or MCP interfaces.

- `--interval <SECONDS>` — positive number of seconds between cycles. Defaults
  to `60`.
- `--json` — print one machine-readable cycle event per completed cycle.

The watcher runs one cycle immediately, then waits for the configured interval
before each subsequent cycle, and continues until Ctrl-C. It checks whether
the recorded server PID is alive; it does not expire leases based on their
age. Dead leases are removed. For a new lease, preservation requires a
verified process identity: the current process must match both the recorded
PID and `process_start_time`. A new lease whose identity could not be resolved
is intentionally unverified/dead and may be pruned even if that numeric PID
currently exists. Legacy leases without identity metadata retain PID-only
liveness behavior. If an agent exits while a new lease's verified recorded
server process remains alive, that lease is preserved.

The initial store-open failure is fatal and exits with the normal unexpected
error status. Errors from ordinary cycles are printed to stderr and retried on
the next interval. Ctrl-C prints a shutdown message to stderr and exits
successfully. Human stdout prints `no leases pruned` for an empty cycle, or
one line per removed lease in the form `pruned port <PORT> (pid <PID>, tag:
<TAG>)`.

JSON mode prints one event object per completed cycle, including empty cycles:

```json
{"event":"watch_cycle","pruned":[]}
```

When leases are removed, `pruned` contains lease views with the same fields as
the `Lease object` shape below. The views describe leases that were dead and
removed, so their `alive` value is `false`:

```json
{"event":"watch_cycle","pruned":[{"port":3000,"pid":57107,"tag":"next-dev","created_at":1785959877,"session":null,"process_start_time":1785959876,"age_secs":3,"alive":false}]}
```

## File intents

File intents record which session is editing which file, keyed by repository plus repo-relative path (`<git-common-dir>:<path>`), so two worktrees of one repository share the key. The OpenCode guard consults them automatically (see [File intents](../README.md#file-intents-opencode)); these commands let a human do the same by hand. State lives in `<data_dir>/intents.json`, written atomically under its own lock.

Sessions are resolved in this order: `--session`, `$PORTZILLA_SESSION`, `$CLAUDE_CODE_SESSION_ID`. `claim` and `release` need one and exit `1` with an instruction when none is available; `who` and `ls` do not.

Paths are resolved against the current directory. `claim` also records the holder's working copy (absolute path, worktree root, repo-relative path), which is what lets a peer prove whether its pending edit merges cleanly.

### `portzilla intent claim <PATH> [--ttl-secs <SECONDS>] [--session <SESSION>] [--json]`

Claims `<PATH>` for the caller's session, or refreshes the caller's existing claim in place (the original `created_at` is kept). A live claim held by another session is refused with exit `1` and the holder's name.

- `--ttl-secs <SECONDS>` — how long the claim stays binding, `1` through `86400`, default `1800` (30 minutes). Zero or above 24 hours is rejected before the store is touched.
- `--session <SESSION>` — overrides the environment fallbacks.
- `--json` — print the intent view instead of a one-line confirmation.

### `portzilla intent who <PATH> [--json]`

Prints the live claim on `<PATH>`. Exits `2` and prints nothing to stdout when the file has no live claim (an expired record is not live).

### `portzilla intent ls [--json]`

Lists every live claim, sorted by key. Human output is a table (`KEY SESSION AGE EXPIRES`, with age and remaining time in seconds); an empty store prints `no live file intents`. JSON output is an array of intent views.

### `portzilla intent release <PATH> [--session <SESSION>] [--json]`

Removes the caller's own claim. A claim held by another session is refused with exit `1` and the holder's name. Releasing a file with no claim is success (exit `0`): release is idempotent, so a retry after a crash needs no special case.

`release --json` has two shapes. A successful release prints the removed claim as a full intent view (the seven fields below). Releasing an absent claim prints only:

```json
{"released":false,"key":"<git-common-dir>:<path>","file":"/repo/src/auth.rs"}
```

Do not assume the seven-field view for every `release --json` result; check for `released` first.

### `portzilla intent history [--session <SESSION>] [--event <EVENT>] [--limit <N>] [--before <SEQ>] [--json]`

Reads the bounded file-intent journal, newest first. Filters combine with logical AND. `--session` matches the claiming session and the losing side of a takeover; `--event` accepts `claim_created`, `claim_refreshed`, `claim_taken_over`, or `released` (an unknown name exits `1` and names the accepted values); `--before` is an exclusive sequence cursor; `--limit` accepts 1 through 1000 and defaults to 100. An empty journal is success and prints `no intent history events` (or the empty page in JSON).

Only successful mutations are recorded, all in the same atomic write as the claim they describe:

| Operation | Event |
|---|---|
| `claim` on a free key | `claim_created` |
| `claim` refreshing a live claim owned by the same session | `claim_refreshed` |
| `claim` taking over an expired claim from another session | `claim_taken_over` (with `previous_session`) |
| `claim` taking over an expired claim from the same session | `claim_refreshed` |
| `release` by the owner | `released` |
| Read, release of an absent key, capacity refusal, validation error | none |

Expired records reaped as a side effect of `claim` are housekeeping and are not logged. Human output is a table (`SEQ AT EVENT SESSION KEY`, with `at` in Unix seconds).

### `portzilla intent history clear`

Removes every retained intent event and prints `cleared N intent history events`. Claims are untouched, and the sequence counter is not reset, so later events never reuse a number. Clear is CLI-only; the MCP `intent_history` tool is read-only.

### Intent history JSON page

```json
{"events":[{"seq":3,"at":1785959900,"event":"released","key":"<git-common-dir>:src/auth.rs","session":"session-a","previous_session":null}],"next_before":null}
```

`events` is newest-first. `next_before` is the cursor to pass as `--before` for the next page, or `null` when no older matching events remain. `previous_session` is non-null only for `claim_taken_over`, where it names the session whose expired claim was replaced.

### Intent JSON view (`claim`, `who`, `ls`, `release`)

```json
{
  "key": "<git-common-dir>:src/auth.rs",
  "file": "/repo/src/auth.rs",
  "session": "session-a",
  "created_at": 1785959877,
  "expires_at": 1785961677,
  "age_secs": 12,
  "live": true
}
```

`key` is opaque; do not parse it (the `:` separator is ambiguous on Windows). `file` is the holder's absolute working copy, and is `null` for a claim written by a v1 binary, which stores no location. `created_at` and `expires_at` are Unix seconds; `age_secs` is `now - created_at`; `live` is `now < expires_at`. `ls` filters to live claims; `who` reports only a live record; `release` also removes a lapsed record the caller owns.

Schema: `intents.json` is a versioned envelope, currently `format_version: 3`. It holds the intents, a bounded journal of at most 10,000 claim/release events (the oldest are trimmed first), and the next event sequence number. v1 files (no `location`) and v2 files are read and upgraded on the next write; their journal starts at sequence 1. Unknown future versions are rejected without modifying the file. An older binary cannot safely read a v3 file: upgrade it before pointing it at the same data directory.

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Success. For `intent release`, also a claim that was already absent; for `intent history`, an empty result |
| `1`  | Unexpected error (I/O failure, corrupt state file, lock failure); for `intent`, also a claim held by another session, a release by a non-owner, a missing session, or an invalid history filter |
| `2`  | Requested record not found: `who` / `release` on a port with no lease, or `intent who` on a file with no live claim |

## JSON output shapes

### Lease object (`ls`, `who`, `release`, `prune`, `watch`)

Used as a single object by `who` and `release`, and as an array of these objects by `ls` and `prune`:

```json
{
  "port": 3001,
  "pid": 57108,
  "tag": "vite-dev",
  "created_at": 1785959877,
  "session": null,
  "process_start_time": 1785959876,
  "age_secs": 3,
  "alive": true
}
```

`created_at` and `process_start_time` are Unix timestamps in seconds. `process_start_time` is omitted for legacy leases or when the platform cannot resolve it. New leases with an unresolved start time are marked internally as unverified and do not count as alive; legacy leases without identity metadata retain PID-only compatibility. `session` is `null` unless `--session` was given at claim time. `alive` reflects a PID and process-start-time check at the moment of the query, not a cached value.

### Claim outcome (`claim --json`)

```json
{
  "port": 3001,
  "pid": 57285,
  "tag": "vite-dev",
  "created_at": 1785959898,
  "session": null,
  "process_start_time": 1785959897,
  "requested_port": 3000,
  "reassigned": true,
  "reassignment_reason": "lease_conflict"
}
```

`requested_port` is the port that was originally asked for; `port` is the port actually leased. `reassigned` is `true` only when `port != requested_port`. When reassigned, `reassignment_reason` is the stable machine-readable value `lease_conflict` or `os_occupied`; it is omitted otherwise.

The persisted `leases.json` records an internal `process_identity_verified` boolean for new claims: `true` when `process_start_time` was resolved and `false` when it was unavailable. The field is omitted for legacy lease records and is not exposed in CLI or MCP JSON views.

## Slot commands

`portzilla slot <command>` manages per-worktree environment slots. The full configuration reference, failure semantics and recipes live in **[SLOTS.md](SLOTS.md)**.

| Command | Arguments | Exit codes |
|---|---|---|
| `slot up <name>` | `--config <path>`, `--replace`, `--json` | 0 created or replaced; 1 invalid config, name invalid, already up without `--replace`, provision or rollback failure |
| `slot env <name>` | `--config <path>` | 0 printed; 1 store or config error; 2 not found |
| `slot run <name> -- <cmd...>` | `--config <path>` | 0 child exited 0; 1 spawn or store error; 2 not found; otherwise the child's code (a signalled child is 1) |
| `slot ls` | `--json` | 0 listed; 1 store error |
| `slot show <name>` | `--json` | 0 shown; 1 store error; 2 not found |
| `slot down <name>` | `--force`, `--json` | 0 torn down; 1 deprovision or release failure; 2 not found |
| `slot prune` | `--json` | 0 pruned or nothing to prune (failures are warnings); 1 store error |

`slot env` prints one sorted `KEY=value` line per variable, safe for `eval "$(portzilla slot env auth)"` and direnv. `slot run` prints only the child's output.

### Slot object (`up`, `ls`, `show`, `down`, `prune --json`)

```json
{"name":"auth","worktree":"/repo/.worktrees/auth","repo_key":"/repo/.git",
 "ports":{"api":3101,"web":3003},
 "db":{"name":"app_auth","schema":"slot_auth"},
 "cache_dirs":["/repo/.worktrees/auth/.slot/auth/target"],
 "env_file":"/repo/.worktrees/auth/.env.portzilla",
 "created_at":1795000000,"age_secs":12}
```

- `db` is `null` when `[slot.db]` is absent; `schema` is omitted when not configured.
- `up` adds `"replaced": bool`.
- `down` adds `"released_ports": [...]`, `"purged_dirs": [...]`, `"warnings": [...]`, `"forced": bool`.
- `ls` and `prune` print arrays, `[]` when empty.

Slots live in their own store, `<data_dir>/slots.json`, versioned and locked the same way as `leases.json`. Ports a slot owns are durable leases: they stay live until released, have no owning process, and appear in `portzilla ls` with pid `0`. The kill guard treats them like any other lease.

## MCP server

`portzilla serve --mcp` runs an [MCP](https://modelcontextprotocol.io) server over stdio, exposing `claim`, `who`, `ls`, `release`, `prune`, read-only `history`, `intent_claim`, `intent_who`, `intent_ls`, `intent_release`, and read-only `intent_history` as MCP tools (those are the registered tool names — no `portzilla_` prefix). This is for AI coding agents with MCP tool access (Claude Code, and any other MCP client): they call `who` the same way they call any other structured tool — typed JSON in, typed JSON out — instead of shelling out to the CLI and parsing text.

Register it with Claude Code:

```console
$ claude mcp add portzilla -- portzilla serve --mcp
```

Every tool's description is written to make the intended behavior explicit to the calling agent — the `claim` tool description, for example, says outright to use it *instead of* killing whatever occupies a port. Tool results use the exact same flat JSON shapes documented above for `--json` output (see JSON output shapes), so anything already written against the CLI's JSON recognizes MCP results too.

- **`claim(port, tag, pid?, session?)`** — same semantics as `portzilla claim`. `pid` is optional here for a different reason than on the CLI: there is no meaningful "parent process" to default to (the MCP client, not a shell, owns the session), so an omitted `pid` falls back to the portzilla server process's own PID — almost never what you want — and the result carries an extra `note` field saying so. Always pass the PID of the process you started (or are about to start) on that port when you have it.

  Reassigned claims include `reassignment_reason` in the structured result: `lease_conflict` when a live lease caused the reassignment, or `os_occupied` when an unregistered process had the requested port bound.
- **`who(port)`** — same semantics as `portzilla who`.
- **`ls()`** — same semantics as `portzilla ls`, no arguments.
- **`intent_claim(path, session, ttl_secs?)`** — claims a file for `session`, recording the holder's working copy so a peer's conflict check can read it. `path` is absolute or relative to the MCP server's working directory; `ttl_secs` must be 1..86400 and defaults to 1800. Returns a tool-level error naming the holder when another live session owns the file.
- **`intent_who(path)`** — the live holder, or a tool-level `not_found` error. Expired claims count as absent.
- **`intent_ls()`** — every live claim, expired ones omitted.
- **`intent_release(path, session)`** — owner-only; a different session gets `not_owner`, and releasing an unclaimed file is a successful no-op (`released: false`).
- **`intent_history(session?, event?, before?, limit?)`** — the same filtered, newest-first file-intent journal query as `portzilla intent history`; read-only.

**Session identity over MCP:** the protocol carries no session id, so the agent supplies one. For own-session recognition to work with the hooks, it must be the same string the harness passes to them (`sessionID` in OpenCode, `session_id` / `CLAUDE_CODE_SESSION_ID` in Claude Code). A different string makes the agent's own claims look foreign.
- **`release(port)`** — same semantics as `portzilla release`, including the still-alive warning (surfaced as a `was_alive` field on the result instead of a stderr line).
- **`prune()`** — same semantics as `portzilla prune`, no arguments.
- **`history(session?, port?, event?, before?, limit?)`** — same filtered, newest-first journal query as `portzilla history`; read-only.

**Errors**: a missing lease (`who`/`release` on a port with no lease) comes back as a *tool-level* error — the JSON-RPC call still succeeds, but the tool result is flagged `isError: true` with a structured `{"error": "not_found", "port": ..., "message": ...}` body. This mirrors the CLI's exit code `2`: it is an expected, well-formed outcome the calling agent should see and act on, not a protocol failure. Actual portzilla failures (I/O errors, corrupt state, lock failures — the CLI's exit code `1`) come back as real JSON-RPC protocol errors instead, since those mean the server itself couldn't do its job.

The MCP server reads and writes the exact same locked `leases.json` the CLI does (respecting `PORTZILLA_DATA_DIR`) — it is a second front end onto the same on-disk state, not a separate store.

## History

`history` reads the bounded operational journal stored with the leases:

```console
$ portzilla history
$ portzilla history --json
$ portzilla history --session my-session --port 3000 --event guard_denied
$ portzilla history --before 42 --limit 50
$ portzilla history clear
```

Results are newest-first. Filters combine with logical AND; `--before` is an exclusive sequence cursor, and `--limit` accepts 1 through 1000. The journal retains at most 10,000 events and trims the oldest entries while preserving monotonically increasing sequence numbers. `history clear` atomically removes prior events and records one `history_cleared` marker. Clear is CLI-only; the MCP `history` tool is read-only.

Recorded event types are lease mutations, `process_exited`, `guard_denied`, `guard_warned`, and `history_cleared`. Raw shell commands and guard allows are intentionally omitted. Guard event persistence is best-effort and cannot weaken the guard decision.

## Input validation

- `claim` rejects port 0 (clap range 1..=65535); the same check exists in `Store::claim` so any other caller (MCP, direct library use) hits it too.
- `tag` is capped at 1024 characters and `session` at 512; oversized values are rejected with a clear error before the store is touched.
- Hook runners cap stdin at 1 MiB; over-cap inputs are treated like a malformed payload.

## Data file location

`portzilla` resolves its data directory in this order:

1. `PORTZILLA_DATA_DIR` environment variable, used directly as the data directory.
2. `$XDG_DATA_HOME/portzilla`.
3. `$HOME/.local/share/portzilla`.

The state lives at `<data_dir>/leases.json`, written atomically (write to a temp file, then rename) and guarded by an exclusive file lock at `<data_dir>/leases.json.lock` for the duration of every read-modify-write operation. New writes use the v3 envelope containing `leases`, `events`, and `next_event_sequence`:

```json
{"format_version":3,"leases":[{"port":3001,"pid":57108,"tag":"vite-dev","created_at":1785959877,"session":null,"process_start_time":1785959876,"process_identity_verified":true}],"events":[],"next_event_sequence":1}
```

Legacy bare arrays and v2 envelopes remain readable and are upgraded when the new binary writes. Unknown or future format versions are refused without modifying the file. Do not point an older portzilla binary at a v3 state file: it does not understand the envelope and must be upgraded first. This guard prevents an older writer from dropping process identity or history fields.

Set `PORTZILLA_DATA_DIR` to isolate tests, CI runs, or throwaway experiments from your real lease store.
