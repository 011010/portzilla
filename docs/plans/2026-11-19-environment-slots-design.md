# Environment slots — design

Date: 2026-11-19
Status: approved

## Problem

Port leases and file intents coordinate two resources between parallel agent sessions. The rest of the runtime is still shared per machine: two worktrees pointed at the same Postgres database run migrations against each other, and two Cargo builds pointed at the same `target/` serialise on Cargo's directory lock ("Blocking waiting for file lock on build directory"). `agentslots` explores this space on macOS with Postgres only; Portzilla already has the lease table, the kill guard, and a multi-platform single binary.

A **slot** is a named bundle of per-worktree resources: ports, cache directories, environment variables, and an optional database, created and destroyed explicitly.

## Scope decision

Three levels were considered: (A) ports and cache directories, (B) A plus databases, (C) B plus containers. **B was chosen.** Containers stay out: that is where `agentslots` and Emdash are already competing, and Docker adds a supervision surface Portzilla has deliberately avoided.

Database isolation uses **configurable commands** rather than built-in Postgres support: Portzilla never holds credentials, never links a database driver, and the same mechanism covers SQLite, Postgres, and MySQL.

## Configuration

`portzilla.toml` at the working tree root (or `--config <path>`):

```toml
[slot]
purge_cache_on_down = true

[slot.ports]
web = 3000        # preferred base; the allocator walks forward from here
api = 3100

[slot.cache]
CARGO_TARGET_DIR = ".slot/{name}/target"
XDG_CACHE_HOME   = ".slot/{name}/cache"

[slot.env]
RUST_LOG = "debug"

[slot.db]
name_template     = "app_{slot}"
schema_template   = "slot_{slot}"
provision         = ["createdb", "{db}"]
deprovision       = ["dropdb", "--if-exists", "{db}"]
```

- Ports are a table of name to preferred base. Allocation walks forward from the base until a port is free by lease **and** by the OS probe, reusing `Store::claim` with the tag `slot:<name>:<port-name>`, so the existing kill guard and `who` work unchanged.
- `provision`/`deprovision` are **argv arrays**, not shell strings: no implicit shell, no quoting rules, and `["sh", "-c", "..."]` is available for people who want one. Placeholders `{slot}`, `{db}`, `{schema}`, `{worktree}` are substituted per element.
- `{name}` is substituted in cache paths; relative paths are resolved against the worktree.
- TOML is read with the `toml` crate — the only new dependency in this slice.

## Commands

| Command | Behaviour |
|---|---|
| `slot up <name>` | Allocates each port, creates cache dirs, provisions the database when configured, writes `.env.portzilla`, persists the slot. Any failure rolls the whole thing back |
| `slot env <name>` | Prints `KEY=value` lines for `eval` or direnv |
| `slot run <name> -- <cmd>` | Runs the command with the slot environment injected, like `run` |
| `slot ls` / `slot show <name>` | State, ports, database, cache dirs, age |
| `slot down <name>` | Runs `deprovision`, releases the ports, purges cache dirs when configured, removes the record |
| `slot prune` | Removes slots whose worktree no longer exists |

`--json` is available on every read command.

## Data model

A third store, `slots.json`, locked and versioned exactly like `leases.json` and `intents.json`:

```json
{
  "format_version": 1,
  "slots": [{
    "name": "auth",
    "worktree": "/repo/.worktrees/auth",
    "repo_key": "/repo/.git",
    "ports": { "web": 3003, "api": 3101 },
    "db": { "name": "app_auth", "schema": "slot_auth" },
    "cache_dirs": ["/repo/.worktrees/auth/.slot/auth/target"],
    "env_file": "/repo/.worktrees/auth/.env.portzilla",
    "created_at": 1795000000
  }]
}
```

Slots are keyed by **(repo, worktree, name)**: the same name in two worktrees is two different slots, because their directories and env files are different. A repeated name in the same worktree is an error.

## Lifecycle decisions

- **Explicit, no TTL and no daemon.** `up` and `down` are the lifecycle. `prune` handles worktrees that disappeared.
- **Rollback on `up`:** if anything fails mid-way, ports are released, directories created by this run are removed, and a database that was provisioned is deprovisioned. Nothing is persisted.
- **`down` is retryable:** if `deprovision` fails, the record, ports, and directories stay and the command exits non-zero, so the user can fix the cause and retry. `--force` cleans up regardless.
- **`up` on an existing slot** errors; `--replace` runs `down` first.
- **`.env.portzilla`** is written in the worktree and never added to `.gitignore` automatically. `init` mentions it; the user decides.
- **No secrets:** provision commands inherit the caller's environment; Portzilla only passes names and paths.

## Failure handling

- Missing or invalid config → clear error, exit 1, nothing created.
- Port allocation failure → rollback.
- Provision failure → rollback, and the provision command's stderr is surfaced verbatim.
- Unknown placeholder in a template → error before anything is created.

## Testing

- Config parsing: valid, missing sections, unknown placeholders, argv vs shell.
- Slot up/down round trip with ports and dirs, plus rollback on a failing provision command (a fake command that exits 1).
- Env rendering: quoting, ordering, computed values, `.env.portzilla` contents.
- CLI: exit codes, `--json`, `--replace`, `down --force`, `prune`.
- End-to-end: two worktrees of a small Cargo project; `slot up` in both; `cargo check` concurrently with `CARGO_TARGET_DIR` per slot shows no lock wait; `down` purges the dirs.

## Out of scope

- Containers, Compose, and volumes.
- Cross-machine slots.
- Automatic `.gitignore` edits.
- A daemon or TTL-based teardown.
- Built-in Postgres support (rejected in favour of configurable commands).
