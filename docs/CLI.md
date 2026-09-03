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

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Success |
| `1`  | Unexpected error (I/O failure, corrupt state file, lock failure) |
| `2`  | Requested lease not found (`who` / `release` on a port with no lease) |

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

## MCP server

`portzilla serve --mcp` runs an [MCP](https://modelcontextprotocol.io) server over stdio, exposing `claim`, `who`, `ls`, `release`, and `prune` as MCP tools (those are the registered tool names — no `portzilla_` prefix). This is for AI coding agents with MCP tool access (Claude Code, and any other MCP client): they call `who` the same way they call any other structured tool — typed JSON in, typed JSON out — instead of shelling out to the CLI and parsing text.

Register it with Claude Code:

```console
$ claude mcp add portzilla -- portzilla serve --mcp
```

Every tool's description is written to make the intended behavior explicit to the calling agent — the `claim` tool description, for example, says outright to use it *instead of* killing whatever occupies a port. Tool results use the exact same flat JSON shapes documented above for `--json` output (see JSON output shapes), so anything already written against the CLI's JSON recognizes MCP results too.

- **`claim(port, tag, pid?, session?)`** — same semantics as `portzilla claim`. `pid` is optional here for a different reason than on the CLI: there is no meaningful "parent process" to default to (the MCP client, not a shell, owns the session), so an omitted `pid` falls back to the portzilla server process's own PID — almost never what you want — and the result carries an extra `note` field saying so. Always pass the PID of the process you started (or are about to start) on that port when you have it.

  Reassigned claims include `reassignment_reason` in the structured result: `lease_conflict` when a live lease caused the reassignment, or `os_occupied` when an unregistered process had the requested port bound.
- **`who(port)`** — same semantics as `portzilla who`.
- **`ls()`** — same semantics as `portzilla ls`, no arguments.
- **`release(port)`** — same semantics as `portzilla release`, including the still-alive warning (surfaced as a `was_alive` field on the result instead of a stderr line).
- **`prune()`** — same semantics as `portzilla prune`, no arguments.

**Errors**: a missing lease (`who`/`release` on a port with no lease) comes back as a *tool-level* error — the JSON-RPC call still succeeds, but the tool result is flagged `isError: true` with a structured `{"error": "not_found", "port": ..., "message": ...}` body. This mirrors the CLI's exit code `2`: it is an expected, well-formed outcome the calling agent should see and act on, not a protocol failure. Actual portzilla failures (I/O errors, corrupt state, lock failures — the CLI's exit code `1`) come back as real JSON-RPC protocol errors instead, since those mean the server itself couldn't do its job.

The MCP server reads and writes the exact same locked `leases.json` the CLI does (respecting `PORTZILLA_DATA_DIR`) — it is a second front end onto the same on-disk state, not a separate store.

## Input validation

- `claim` rejects port 0 (clap range 1..=65535); the same check exists in `Store::claim` so any other caller (MCP, direct library use) hits it too.
- `tag` is capped at 1024 characters and `session` at 512; oversized values are rejected with a clear error before the store is touched.
- Hook runners cap stdin at 1 MiB; over-cap inputs are treated like a malformed payload.

## Data file location

`portzilla` resolves its data directory in this order:

1. `PORTZILLA_DATA_DIR` environment variable, used directly as the data directory.
2. `$XDG_DATA_HOME/portzilla`.
3. `$HOME/.local/share/portzilla`.

The state lives at `<data_dir>/leases.json`, written atomically (write to a temp file, then rename) and guarded by an exclusive file lock at `<data_dir>/leases.json.lock` for the duration of every read-modify-write operation. New writes use this envelope:

```json
{"format_version":2,"leases":[{"port":3001,"pid":57108,"tag":"vite-dev","created_at":1785959877,"session":null,"process_start_time":1785959876,"process_identity_verified":true}]}
```

Legacy bare arrays remain readable and are upgraded when the new binary writes. Unknown or future format versions are refused without modifying the file. Do not point an older portzilla binary at a v2 state file: it does not understand the envelope and must be upgraded first. This guard prevents an older writer from dropping process identity fields.

Set `PORTZILLA_DATA_DIR` to isolate tests, CI runs, or throwaway experiments from your real lease store.
