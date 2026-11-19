# Environment slots — implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `portzilla slot up|env|run|ls|show|down|prune` so one worktree gets its own ports, cache directories, environment file, and database, created and destroyed explicitly.

**Architecture:** A new `portzilla.toml` config, a new locked `slots.json` store, and a command surface that allocates ports through the existing lease store (`tag = "slot:<name>:<port>"`) and provisions databases by running configured argv commands. Failure rolls `up` back completely.

**Tech Stack:** Rust 2024, `toml` (the only new dependency), existing `store`/`lease`/`intent` modules, `assert_cmd`.

**Design doc:** `docs/plans/2026-11-19-environment-slots-design.md`

---

## Global constraints

- **No new dependencies except `toml`.** Verify with `git diff Cargo.toml` that nothing else appears.
- **Provision commands are argv arrays.** Never pass them through a shell.
- **Rollback is mandatory on `up`:** ports released, directories created by this run removed, provisioned database deprovisioned, nothing persisted.
- **`down` keeps the record on deprovision failure** unless `--force`.
- **Fork guard:** any new `std::process::Command` git call keeps the `cfg(test)` lock (slots do not need git beyond `locate`-style resolution, so this should not appear).
- Conventional commits, no AI attribution, never stage `.gitignore`, `docs/CLI.md`, `.atl/`, `.playwright-mcp/`, `graphify-out/`.

---

### Task 1: Config parsing

**Files:** Create `src/slot_config.rs`; modify `src/main.rs` (`mod slot_config;`); tests inside the module.

**Step 1: failing tests**

```rust
#[test]
fn parses_ports_cache_env_and_db() { /* full example from the design doc */ }

#[test]
fn a_missing_file_is_a_clear_error() { /* message names the expected path */ }

#[test]
fn an_unknown_placeholder_is_rejected_before_anything_runs() { /* "{nope}" in provision */ }

#[test]
fn relative_cache_paths_resolve_against_the_worktree() { /* ".slot/{name}/target" */ }

#[test]
fn provision_is_argv_not_a_shell_string() { /* a string value fails to deserialize */ }
```

**Step 2:** run `cargo test -p portzilla slot_config` and confirm failure.

**Step 3: implement**

```rust
pub struct SlotConfig {
    pub purge_cache_on_down: bool,
    pub ports: BTreeMap<String, u16>,        // name -> preferred base
    pub cache: BTreeMap<String, String>,     // env var -> path template
    pub env: BTreeMap<String, String>,
    pub db: Option<DbConfig>,
}

pub struct DbConfig {
    pub name_template: String,
    pub schema_template: Option<String>,
    pub provision: Vec<String>,
    pub deprovision: Option<Vec<String>>,
}
```

- `SlotConfig::load(worktree: &Path, explicit: Option<&Path>)` reads `portzilla.toml`.
- Placeholders allowed: `{name}` (cache paths), `{slot}`, `{db}`, `{schema}`, `{worktree}` (db commands). Any other `{...}` → error naming the offending token.
- Add `toml = "0.9"` (or the current minor) to `Cargo.toml` with `features = ["parse", "serde"]` if needed.

**Step 4:** tests green; commit `feat(slot): parse portzilla.toml`.

---

### Task 2: The slot store

**Files:** Create `src/slot.rs` (store + types); modify `src/main.rs`.

**Step 1: failing tests**

```rust
#[test]
fn up_then_get_round_trips_a_slot() { /* ports, db, cache dirs, env file, created_at */ }

#[test]
fn a_second_slot_with_the_same_name_in_one_worktree_is_refused() { /* clear error */ }

#[test]
fn the_same_name_in_two_worktrees_coexists() { /* two records */ }

#[test]
fn an_unknown_format_version_is_rejected_without_rewriting() { /* like intents.json */ }

#[test]
fn prune_removes_slots_whose_worktree_is_gone() { /* and keeps the rest */ }
```

**Step 2:** confirm failure.

**Step 3: implement**

- `SlotRecord { name, worktree, repo_key, ports: BTreeMap<String,u16>, db: Option<DbRuntime>, cache_dirs: Vec<PathBuf>, env_file: PathBuf, created_at }`.
- `SlotStore` mirroring `IntentStore`: `slots.json`, `fs4` lock, versioned envelope `{format_version: 1, slots: [...]}`, atomic temp+rename, `PORTZILLA_DATA_DIR` resolution, permission hardening.
- `insert`, `get(worktree, name)`, `list`, `remove`, `prune(now)`.
- `repo_key`: reuse the detection that `key_for_file` already performs (common dir), exposed as a helper rather than reimplemented.

**Step 4:** tests green; commit `feat(slot): persist slots in a locked store`.

---

### Task 3: `slot up` with ports, cache dirs and env file

**Files:** Create `src/slot_runtime.rs` (allocation, env rendering, rollback); modify `src/main.rs` (CLI).

**Step 1: failing tests**

```rust
#[test]
fn up_allocates_a_port_per_name_and_claims_it_as_a_lease() { /* tag slot:<name>:<port> */ }

#[test]
fn up_walks_forward_when_the_preferred_port_is_taken() { /* lease or OS */ }

#[test]
fn up_creates_cache_dirs_and_writes_the_env_file() { /* .env.portzilla has every key */ }

#[test]
fn a_failure_after_allocation_releases_the_ports_and_removes_created_dirs() { /* rollback */ }

#[test]
fn env_rendering_is_stable_and_quotes_values_with_spaces() { /* KEY="a b" */ }
```

**Step 2:** confirm failure.

**Step 3: implement**

- Allocation: for each `(port_name, base)`, walk forward from `base` calling `Store::claim` with `tag = format!("slot:{slot}:{port}")` and the slot name as session; verification that the returned port equals the requested base is not required — `claim` may reassign, which is the walk.
- Cache dirs: render `{name}`, resolve relative against the worktree, `create_dir_all`, record them.
- Env rendering (shared with `slot env` and `slot run`):
  - `PORTZILLA_SLOT`, `PORTZILLA_WORKTREE`
  - `PORTZILLA_PORT_<UPPER_NAME>` for each port, plus `PORTZILLA_PORT` = first port in name order
  - `PORTZILLA_DB`, `PORTZILLA_DB_SCHEMA` when configured
  - `[slot.env]` values, then `[slot.cache]` values (cache wins on conflict; document that)
- `.env.portzilla`: one `KEY=value` per line, values quoted when they contain spaces or `#`.
- Rollback: a guard struct that releases claimed ports, removes directories this run created (only those that did not exist before), and runs deprovision if provisioning succeeded.

**Step 4:** tests green; commit `feat(slot): bring a slot up with ports, caches and an env file`.

---

### Task 4: Databases

**Files:** `src/slot_runtime.rs`; tests.

**Step 1: failing tests**

```rust
#[test]
fn provision_receives_the_substituted_argv_and_inherits_the_environment() {
    /* provision = ["sh","-c","printf %s \"$PORTZILLA_DB\" > db.txt"] */
}

#[test]
fn a_failing_provision_command_rolls_the_slot_back() { /* exit 1 -> no record, ports free, dirs gone */ }

#[test]
fn down_runs_deprovision_then_releases_ports_and_purges_dirs() { /* order asserted via files */ }

#[test]
fn down_keeps_everything_when_deprovision_fails() { /* record, ports, dirs intact; exit != 0 */ }

#[test]
fn down_force_cleans_up_even_when_deprovision_fails() { /* record gone */ }
```

**Step 2:** confirm failure.

**Step 3: implement**

- Render `{slot}`, `{db}`, `{schema}`, `{worktree}` per argv element; export `PORTZILLA_DB`/`PORTZILLA_DB_SCHEMA` to the child.
- Run provision with inherited env; capture stderr; on failure surface it and roll back.
- `down`: run deprovision first (if configured), then release ports, purge dirs when `purge_cache_on_down`, remove the record. On deprovision failure: keep everything unless `--force`.

**Step 4:** tests green; commit `feat(slot): provision and deprovision the slot database`.

---

### Task 5: CLI surface

**Files:** `src/main.rs`, `tests/cli.rs`.

Commands (clap nested enum under `Commands::Slot`):

```rust
enum SlotCommands {
    Up { name: String, #[arg(long)] config: Option<PathBuf>, #[arg(long)] replace: bool, #[arg(long)] json: bool },
    Env { name: String, #[arg(long)] config: Option<PathBuf> },
    Run { name: String, #[arg(long)] config: Option<PathBuf>, #[arg(last = true)] command: Vec<String> },
    Ls { #[arg(long)] json: bool },
    Show { name: String, #[arg(long)] json: bool },
    Down { name: String, #[arg(long)] force: bool, #[arg(long)] json: bool },
    Prune { #[arg(long)] json: bool },
}
```

Exit codes: 0 success; 1 error (already up without `--replace`, provision failure, deprovision failure); 2 slot not found for `show`/`down`/`env`/`run`.

Tests: round trip, `--replace`, `--json` shapes, exit 2, `run` injects `PORTZILLA_PORT_*`, `env` output is `eval`-able, `prune`.

Commit `feat(cli): add the slot command surface`.

---

### Task 6: Documentation

**Files:** `README.md`, new `docs/SLOTS.md`, `docs/CLI.md`, `CHANGELOG.md`, `docs/ROADMAP.md`.

- README: short section and a link; support the "what it does not do" list (no containers, no daemon, no secrets).
- `docs/SLOTS.md`: full config reference, the SQLite and Postgres recipes (including the `search_path` note), rollback and failure semantics, how slots interact with the kill guard and file intents.
- CLI.md: every subcommand, exit codes, JSON shapes.
- CHANGELOG under `Unreleased`.

Commit `docs: document environment slots`.

---

### Task 7: End-to-end

**Files:** none.

**Step 1:** two linked worktrees of a small Cargo crate, one `portzilla.toml` copied into both with `[slot.cache] CARGO_TARGET_DIR` and two ports.

**Step 2:** `slot up a` in worktree A and `slot up b` in worktree B; assert both get different ports and different `CARGO_TARGET_DIR`.

**Step 3:** run `cargo check` concurrently in both via `slot run`; assert neither prints "Blocking waiting for file lock on build directory" and that two distinct target directories exist.

**Step 4:** `slot down a`; assert A's target dir is gone, the port is free, and `slot ls` shows only B. Then `slot down b`.

**Step 5:** clean up the temporary repos and the isolated `PORTZILLA_DATA_DIR`.

---

## Out of scope

- Containers/Compose.
- Cross-machine slots.
- Automatic `.gitignore` edits.
- TTL-based teardown or a daemon.
- Built-in Postgres support.
