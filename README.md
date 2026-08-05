# portzilla

`portzilla` is a lease registry for localhost ports. It exists because AI coding agents (Claude Code, Cursor, and similar tools) increasingly run several parallel sessions on one machine — each in its own git worktree, each starting its own dev server — and today those sessions have no way to coordinate. An agent that finds port 3000 busy typically just kills whatever is on it, which can be a sibling session's dev server. `portzilla` gives processes a way to claim a port with an owner PID and a purpose tag, gives conflicting claims the next free port instead of stealing, lets anyone ask who owns a port before killing it, and prunes leases whose owning process has died. All output is available as JSON so agents can consume it directly, and the tool works the same way for a human typing commands in a terminal.

This is coordination, not diagnostics: existing tools like `witr`, `kill-port`, and ServerSlayer tell you what is on a port or kill it after the fact. `portzilla` is the layer that prevents the conflict in the first place.

## 30-second demo

```console
$ portzilla claim 3000 --tag next-dev --pid 57107
claimed port 3000 for pid 57107 (tag: next-dev)

$ portzilla claim 3000 --tag vite-dev --pid 57108
port 3000 is busy; claimed port 3001 instead for pid 57108 (tag: vite-dev)

$ portzilla who 3001
port: 3001
pid: 57108
tag: vite-dev
session: (none)
age: 3s
status: alive

$ portzilla who 3001 --json
{"port":3001,"pid":57108,"tag":"vite-dev","created_at":1785959877,"session":null,"age_secs":3,"alive":true}

$ portzilla ls
PORT    PID      STATUS AGE        TAG
3000    57107    alive  5s         next-dev
3001    57108    alive  3s         vite-dev

$ portzilla release 3001
warning: released port 3001 whose owning pid 57108 is still alive
port: 3001
pid: 57108
tag: vite-dev
session: (none)
age: 3s
status: alive

$ portzilla prune
no dead leases to prune
```

The output above is real: it was captured by running the built binary with `PORTZILLA_DATA_DIR` pointed at a temporary directory holding two genuinely live processes.

## Install

```console
$ cargo install portzilla
```

Not yet published to crates.io — until then, build from source:

```console
$ cargo install --path .
```

## Command reference

Every command accepts `--json` to print a single JSON value on stdout instead of human-readable text. Every command reads and writes the same file-locked JSON state file, so concurrent invocations from different processes are serialized safely.

### `portzilla claim <PORT> --tag <TAG> [--pid <PID>] [--session <SESSION>] [--json]`

Claims `<PORT>` for the given (or default) PID and tag.

- `--tag <TAG>` — required. Human-readable description of what the port is for.
- `--pid <PID>` — optional. Defaults to the PID of the parent process (the shell or agent invoking `portzilla`); falls back to `portzilla`'s own PID if the parent cannot be determined.
- `--session <SESSION>` — optional. Groups related leases under a session identifier.
- `--json` — print the outcome as JSON instead of a one-line summary.

Conflict resolution:

- No lease on the requested port, or the existing lease's owning PID is dead: the port is claimed (or re-taken) directly. Not reassigned.
- A live lease on the requested port already owned by the same PID: the lease is updated in place (idempotent re-claim — tag and session can change, the port does not). Not reassigned.
- A live lease on the requested port owned by a different PID: `portzilla` finds the next free port at or after `requested_port + 1` (skipping ports with a live lease and ports the OS reports as bound) and claims that instead. Reassigned.

### `portzilla ls [--json]`

Lists every recorded lease. Human output is a table (`PORT PID STATUS AGE TAG`); JSON output is an array of lease objects (see shape below).

### `portzilla who <PORT> [--json]`

Shows the lease recorded on `<PORT>`. Exits with code `2` and prints nothing to stdout if no lease exists on that port.

### `portzilla release <PORT> [--json]`

Removes the lease recorded on `<PORT>` and prints the removed lease. Exits with code `2` if no lease exists on that port. If the owning PID is still alive at the time of release, a warning is printed to stderr (`release` always wins — it does not check ownership or refuse to act).

### `portzilla prune [--json]`

Removes every lease whose owning PID is no longer alive and prints each one that was removed. Human output prints `no dead leases to prune` if nothing was pruned; JSON output prints `[]`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0`  | Success |
| `1`  | Unexpected error (I/O failure, corrupt state file, lock failure) |
| `2`  | Requested lease not found (`who` / `release` on a port with no lease) |

## JSON output shapes

### Lease object (`ls`, `who`, `release`, `prune`)

Used as a single object by `who` and `release`, and as an array of these objects by `ls` and `prune`:

```json
{
  "port": 3001,
  "pid": 57108,
  "tag": "vite-dev",
  "created_at": 1785959877,
  "session": null,
  "age_secs": 3,
  "alive": true
}
```

`created_at` is a Unix timestamp in seconds. `session` is `null` unless `--session` was given at claim time. `alive` reflects a live PID-table check at the moment of the query, not a cached value.

### Claim outcome (`claim --json`)

```json
{
  "port": 3001,
  "pid": 57285,
  "tag": "vite-dev",
  "created_at": 1785959898,
  "session": null,
  "requested_port": 3000,
  "reassigned": true
}
```

`requested_port` is the port that was originally asked for; `port` is the port actually leased. `reassigned` is `true` only when `port != requested_port` because of a live conflicting claim.

## MCP server

`portzilla serve --mcp` runs an [MCP](https://modelcontextprotocol.io) server over stdio, exposing `claim`, `who`, `ls`, `release`, and `prune` as MCP tools (those are the registered tool names — no `portzilla_` prefix). This is for AI coding agents with MCP tool access (Claude Code, and any other MCP client): they call `who` the same way they call any other structured tool — typed JSON in, typed JSON out — instead of shelling out to the CLI and parsing text.

Register it with Claude Code:

```console
$ claude mcp add portzilla -- portzilla serve --mcp
```

Every tool's description is written to make the intended behavior explicit to the calling agent — the `claim` tool description, for example, says outright to use it *instead of* killing whatever occupies a port. Tool results use the exact same flat JSON shapes documented above for `--json` output (see [JSON output shapes](#json-output-shapes)), so anything already written against the CLI's JSON recognizes MCP results too.

- **`claim(port, tag, pid?, session?)`** — same semantics as `portzilla claim`. `pid` is optional here for a different reason than on the CLI: there is no meaningful "parent process" to default to (the MCP client, not a shell, owns the session), so an omitted `pid` falls back to the portzilla server process's own PID — almost never what you want — and the result carries an extra `note` field saying so. Always pass the PID of the process you started (or are about to start) on that port when you have it.
- **`who(port)`** — same semantics as `portzilla who`.
- **`ls()`** — same semantics as `portzilla ls`, no arguments.
- **`release(port)`** — same semantics as `portzilla release`, including the still-alive warning (surfaced as a `was_alive` field on the result instead of a stderr line).
- **`prune()`** — same semantics as `portzilla prune`, no arguments.

**Errors**: a missing lease (`who`/`release` on a port with no lease) comes back as a *tool-level* error — the JSON-RPC call still succeeds, but the tool result is flagged `isError: true` with a structured `{"error": "not_found", "port": ..., "message": ...}` body. This mirrors the CLI's exit code `2`: it is an expected, well-formed outcome the calling agent should see and act on, not a protocol failure. Actual portzilla failures (I/O errors, corrupt state, lock failures — the CLI's exit code `1`) come back as real JSON-RPC protocol errors instead, since those mean the server itself couldn't do its job.

The MCP server reads and writes the exact same locked `leases.json` the CLI does (respecting `PORTZILLA_DATA_DIR`) — it is a second front end onto the same on-disk state, not a separate store.

## Data file location

`portzilla` resolves its data directory in this order:

1. `PORTZILLA_DATA_DIR` environment variable, used directly as the data directory.
2. `$XDG_DATA_HOME/portzilla`.
3. `$HOME/.local/share/portzilla`.

The state lives at `<data_dir>/leases.json`, a pretty-printed JSON array of lease objects, written atomically (write to a temp file, then rename) and guarded by an exclusive file lock at `<data_dir>/leases.json.lock` for the duration of every read-modify-write operation.

Set `PORTZILLA_DATA_DIR` to isolate tests, CI runs, or throwaway experiments from your real lease store.

## Limitations

**PID reuse.** Liveness is determined by asking the OS process table "does a process with this PID exist right now?" (via `sysinfo`). Operating systems recycle PIDs. If a leased process dies and, before that lease is pruned or released, a new unrelated process happens to be assigned the same PID, `portzilla` will report the old lease as `alive` — a false positive. This is a known tradeoff of not running a background daemon: there is no process to actively watch for exit events, only a point-in-time PID check on each query. `portzilla prune` mitigates this by letting you sweep dead leases on demand; an active daemon is on the [roadmap](docs/ROADMAP.md) to close this gap for good.

`portzilla` does not manage processes (it will not start or stop a dev server for you), does not enforce a firewall or sandbox, and does not coordinate ports across machines — see [`docs/PRD.md`](docs/PRD.md) for the full list of non-goals.

## Roadmap

v0.1 covers `claim`, `ls`, `who`, `release`, `prune`, JSON output, and locked local state. v0.1.x adds the MCP server (`serve --mcp`, documented above) so agents can call `portzilla` natively instead of shelling out. Planned next: a Claude Code hook that intercepts kill/lsof-style patterns and suggests a `portzilla who` check instead. See [`docs/ROADMAP.md`](docs/ROADMAP.md) for the full versioned plan.

## License

MIT. See [`LICENSE`](LICENSE).
