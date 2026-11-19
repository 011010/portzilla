# Environment slots

A **slot** is a named bundle of per-worktree resources: ports, cache directories, an environment file, and optionally a database. It exists so two agent sessions working in parallel can each own a complete, non-overlapping environment.

Without slots, two worktrees of one project share the machine: both point at the same Postgres database, and both build into the same `target/`, where Cargo's directory lock serialises them ("Blocking waiting for file lock on build directory").

Slots are the third layer of Portzilla. Port leases coordinate individual ports; file intents coordinate edits; slots coordinate a whole worktree environment.

## What a slot does and does not do

**Does:**

- Allocates one port per configured name, recorded as a normal (durable) lease.
- Creates per-slot cache directories and exports them as environment variables.
- Writes `.env.portzilla` in the worktree.
- Runs configured commands to create and destroy a database (or anything else) per slot.
- Rolls back completely when `up` fails.

**Does not:**

- Start, stop, or supervise services. Slots prepare an environment; `slot run` runs one command with it.
- Run containers. Compose, Docker volumes, and images are out of scope.
- Keep a daemon or expire slots on a timer. `up` and `down` are explicit.
- Store credentials. Provision commands inherit the caller's environment.

## Configuration

`portzilla.toml` at the worktree root, or the path passed with `--config`:

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
name_template    = "app_{slot}"
schema_template  = "slot_{slot}"
provision        = ["createdb", "{db}"]
deprovision      = ["dropdb", "--if-exists", "{db}"]
```

### Rules

- **Ports** is a table of name → preferred base port. Allocation starts at the base and walks forward until a port is free both by lease and by the OS probe. A slot's ports are leased with the tag `slot:<slot>:<port-name>`, so `portzilla who <port>` and the kill guard see them like any other lease.
- **Cache paths** must resolve inside the worktree; a path outside it is refused. Two slots may not record overlapping cache paths.
- **`purge_cache_on_down`** removes the recorded cache directories on `down`. When false they survive, which is usually what you want for a Cargo target dir.
- **`provision` / `deprovision` are argv arrays**, never shell strings. `["sh", "-c", "..."]` is available when you want a shell, but it is explicit.
- **Placeholders**: `{name}` in cache paths; `{slot}`, `{db}`, `{schema}`, `{worktree}` in database commands. An unknown placeholder is rejected before anything runs. Shell syntax inside an argv element is data, so `awk '{print $1}'` is fine.
- **Empty `provision`/`deprovision` arrays** are no-ops, which is what a SQLite recipe wants.

## Commands

```console
$ portzilla slot up auth
$ eval "$(portzilla slot env auth)"
$ portzilla slot run auth -- npm test
$ portzilla slot ls
$ portzilla slot show auth
$ portzilla slot down auth
$ portzilla slot prune
```

| Command | Notes |
|---|---|
| `slot up <name>` | Unique within the worktree. `--replace` tears down an existing one first |
| `slot env <name>` | `KEY=value` lines, sorted, safe for `eval` and direnv |
| `slot run <name> -- <cmd>` | Runs `<cmd>` with the slot environment; forwards its exit code |
| `slot ls` | Every slot in every worktree, with `--json` |
| `slot show <name>` | One slot, with `--json` |
| `slot down <name>` | Deprovision, release ports, purge caches when configured, remove the record. `--force` cleans up even when deprovision fails |
| `slot prune` | Removes slots whose worktree disappeared, deprovisioning first (best effort, warnings on failure) |

Exit codes: `0` success, `1` error (invalid config, already up without `--replace`, provision or deprovision failure), `2` slot not found for `env`, `run`, `show` and `down`.

## Environment

Every slot exports:

| Variable | Value |
|---|---|
| `PORTZILLA_SLOT` | slot name |
| `PORTZILLA_WORKTREE` | absolute worktree path |
| `PORTZILLA_PORT_<NAME>` | one per port, upper-cased with `-` → `_` |
| `PORTZILLA_PORT` | the first port in alphabetical name order |
| `PORTZILLA_DB` | rendered `name_template`, when configured |
| `PORTZILLA_DB_SCHEMA` | rendered `schema_template`, when configured |
| `[slot.env]` entries | verbatim |
| `[slot.cache]` entries | resolved absolute paths (these win over `[slot.env]` on conflict) |

## Recipes

`[slot.env]` values are **literal**: they are exported exactly as written, with no placeholder or shell expansion. Anything that has to vary per slot comes from the variables Portzilla exports — `PORTZILLA_SLOT`, `PORTZILLA_WORKTREE`, `PORTZILLA_DB`, `PORTZILLA_DB_SCHEMA`, `PORTZILLA_PORT_*` — read by the application or by a `slot run` wrapper.

**SQLite** needs no provisioning. Build the URL in a wrapper:

```console
$ portzilla slot run auth -- sh -c 'DATABASE_URL="sqlite://$PORTZILLA_WORKTREE/.slot/$PORTZILLA_SLOT/app.db" exec npm test'
```

**Postgres, one database per slot:**

```toml
[slot.db]
name_template = "app_{slot}"
provision     = ["createdb", "{db}"]
deprovision   = ["dropdb", "--if-exists", "{db}"]
```

```console
$ portzilla slot run auth -- sh -c 'DATABASE_URL="postgres://localhost/$PORTZILLA_DB" exec npm test'
```

**Postgres, one schema per slot** — cheaper, but the connection has to point at the schema:

```toml
[slot.db]
name_template    = "app_shared"
schema_template  = "slot_{slot}"
provision        = ["psql", "-d", "{db}", "-c", "CREATE SCHEMA IF NOT EXISTS {schema}"]
deprovision      = ["psql", "-d", "{db}", "-c", "DROP SCHEMA IF EXISTS {schema} CASCADE"]
```

```console
$ portzilla slot run auth -- sh -c 'DATABASE_URL="postgres://localhost/$PORTZILLA_DB?options=-csearch_path%3D$PORTZILLA_DB_SCHEMA" exec npm test'
```

Most ORMs accept that URL as-is; verify against yours before relying on it.

Provision and deprovision run with the caller's environment, so use `.pgpass`, `PG*` variables, or your normal credential setup. Portzilla never reads or stores them. Interactive password prompts are not forwarded; a command that needs a TTY will hang or fail.

## Lifecycle and failure semantics

- **`up` is transactional.** If anything fails — a port cannot be allocated, a directory cannot be created, `provision` exits non-zero — every claimed port is released, every directory created by that run is removed, a database that was provisioned is deprovisioned, a previous `.env.portzilla` is restored, and nothing is persisted.
- **`down` is retryable.** If `deprovision` fails, the record, ports, and directories stay and the command exits non-zero so you can fix the cause and retry. `--force` removes the record anyway.
- **`up` on an existing slot** errors; `--replace` runs `down` first, and a failed teardown leaves the old slot intact.
- **SIGKILL or power loss between side effects and the final write** can leave durable leases and directories with no slot record. There is no journal. `portzilla ls` shows the leftover `slot:<name>:<port>` leases, and `portzilla release <port>` clears them; delete the directories by hand.

## Limits worth knowing

- **The Cargo registry cache stays shared, on purpose.** A slot isolates the build directory, not `$CARGO_HOME`: concurrent builds may still print "Blocking waiting for file lock on package cache" while they read the shared registry. That lock is short and does not serialise compilation, which is what the build-directory lock does.
- **One `.env.portzilla` per worktree.** Two slots in the same worktree are allowed but the last `up` wins; `slot up` warns when it overwrites another slot's file. `slot env` always reflects the slot you name.
- **Slot tags are not worktree-qualified.** Two slots named `auth` in different worktrees both show `slot:auth:web` in `portzilla ls`. Teardown is still correct: it releases by recorded port and full tag.
- **`down` leaves empty parent directories** (`.slot/<name>`, `.slot`); only recorded cache directories are purged.
- **`.env.portzilla` is never added to `.gitignore` automatically.** Do it yourself if you keep the file.
- **Durable leases have no process.** `portzilla ls` shows them with pid `0` and `alive`, because they stay live until released rather than until a process exits.

## Interaction with the other layers

- **Kill guard:** a slot's ports are ordinary leases with a slot tag, so a foreign agent that tries to kill whatever holds them is denied exactly like any other leased process.
- **File intents:** independent. A slot does not claim files; use the OpenCode or Claude Code hook for that.
- **Concurrency:** `up`, `down`, and `prune` hold a cross-process operation lock, so two agents cannot create the same slot or race over the same cache directory.
