# Portzilla Run And Agent Skill Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Let agents start a server on a safely claimed port whose lease follows the actual server process, then distribute an explicit skill that uses this workflow.

**Architecture:** `portzilla run` claims a port for its wrapper PID, starts the server with `PORTZILLA_PORT`, and atomically transfers the lease to the verified child PID before waiting for that child. A private Store API owns the transfer invariant. The checked-in skill is embedded into `portzilla init skill`, so Cargo and npm installs can print the same portable skill without writing configuration.

**Tech Stack:** Rust 2024, clap, sysinfo, fs4 file locking, assert_cmd integration tests, Markdown agent skills.

---

### Task 1: Add an identity-checked lease transfer primitive

**Files:**
- Modify: `src/store.rs:91-238, 390-478`
- Modify: `src/lease.rs:23-66`
- Test: `src/store.rs` test module

**Step 1: Write failing transfer tests**

Add focused unit tests for a new internal transfer operation:

- It replaces a lease's PID and process-start identity when the expected wrapper PID and identity still own a live lease.
- It preserves the port, tag, and session.
- It rejects a missing lease, a changed wrapper PID, a stale wrapper identity, a dead child, and a child with no start-time identity.
- It never changes the lease on rejection.

Use a deterministic `PidChecker` fixture that returns distinct start times for wrapper and child PIDs.

**Step 2: Run the focused tests and verify they fail**

Run: `cargo test store::tests::transfer`

Expected: compilation failure because the transfer operation does not exist.

**Step 3: Implement the minimal Store operation**

Add a `pub(crate)` Store method that takes:

```rust
port: u16,
expected_owner_pid: u32,
expected_owner_start_time: u64,
new_owner_pid: u32,
checker: &dyn PidChecker,
```

Under the existing exclusive lock, it must:

1. Read the lease on `port`.
2. Confirm its PID and recorded start time match the expected wrapper identity and that it is alive.
3. Confirm the child PID is alive and has a resolvable start time.
4. Replace only the PID, process-start identity, identity-verification marker, and renewal timestamp while preserving port, tag, and session.
5. Write the state atomically and return the transferred lease.

Keep this API private to the crate. Do not expose ownership transfer through CLI or MCP because an arbitrary caller must not be able to take another process's live lease.

**Step 4: Run the focused tests and verify they pass**

Run: `cargo test store::tests::transfer`

Expected: all transfer tests pass.

**Step 5: Run Store regression tests**

Run: `cargo test store::tests`

Expected: existing claim, release, liveness, state-format, and locking tests remain green.

### Task 2: Add the `portzilla run` CLI command

**Files:**
- Modify: `src/main.rs:17-22, 77-176, 294-365`
- Test: `tests/cli.rs`

**Step 1: Add command-parser tests first**

Add CLI tests that verify:

- `run` rejects an absent command after `--`.
- A free requested port launches the child with `PORTZILLA_PORT` set to that port.
- A conflicting requested port launches the child with the reassigned port, not the requested port.
- The lease visible while the child is running names the child PID and has a live process identity.
- A child exit leaves a dead lease that `prune` removes.

Build the child fixture without assuming a shell. Use a platform-aware test helper command that writes its inherited environment and stays alive until the test terminates it.

**Step 2: Run the new CLI tests and verify they fail**

Run: `cargo test --test cli run_`

Expected: clap reports that `run` is unknown.

**Step 3: Add the clap variant**

Add a `Run` command variant with:

```rust
Run {
    port: u16,
    tag: String,
    session: Option<String>,
    command: Vec<String>,
}
```

Use the same port range, tag, and session semantics as `claim`. Mark `command` as `last`, `required`, and `num_args = 1..` so everything after `--` is executed directly without shell interpretation.

**Step 4: Implement launch and transfer behavior**

In the `Commands::Run` branch:

1. Open the Store and claim the requested port for `std::process::id()`.
2. Require the returned wrapper lease to have a verified process start time. If it does not, return an error before starting the child.
3. Print the assigned port and any reassignment note to stderr only. Do not add `--json`: the child owns stdout.
4. Spawn the direct command with inherited stdio, `PORTZILLA_PORT` set to the actual port, and `PORTZILLA_SESSION` set only when the option was supplied.
5. Transfer the live wrapper lease to the spawned child through the new Store method.
6. If spawning or transfer fails, terminate and reap the child when one exists, return an error, and never release a lease without matching the wrapper identity.
7. Wait for the child and return its exit status to the caller. Preserve its nonzero status instead of converting it into Portzilla's generic exit code.

Adjust the top-level command result plumbing only as much as required to carry a child exit code. Do not change existing CLI exit-code behavior for `who`, `release`, guard, MCP, or hook commands.

**Step 5: Run the new command tests**

Run: `cargo test --test cli run_`

Expected: all `run` cases pass, including reassignment and observed child ownership.

**Step 6: Run command regressions**

Run: `cargo test --test cli`

Expected: all existing CLI behavior remains green.

### Task 3: Verify lifecycle and guard behavior end to end

**Files:**
- Modify: `tests/cli.rs`
- Modify: `tests/guard_bypass.rs` only if a direct helper is needed

**Step 1: Write the failing lifecycle test**

Start a long-lived fixture with `portzilla run`, wait until it reports readiness, then assert:

- `portzilla who <actual-port> --json` reports the fixture child PID as alive.
- A separate session's `portzilla guard -- kill <child-pid>` is denied.
- The original wrapper PID is not recorded as the lease owner.

Terminate the fixture with the existing condition-based reaping helper; do not add fixed sleeps.

**Step 2: Run the lifecycle test and verify it fails before the implementation is complete**

Run: `cargo test --test cli run_transfers_lease_to_child`

Expected: FAIL until the lease transfer and child process handling are implemented.

**Step 3: Make the smallest implementation adjustment required**

Only if Task 2 does not satisfy the test, fix the observed issue in the run branch or private Store transfer. Do not loosen guard matching or bypass identity checks to make the test pass.

**Step 4: Run lifecycle and guard verification**

Run: `cargo test --test cli run_ && cargo test --test guard_bypass`

Expected: run lifecycle tests and guard-bypass tests pass.

### Task 4: Ship an explicit portable Portzilla skill

**Files:**
- Create: `skills/portzilla/SKILL.md`
- Modify: `src/main.rs:232-258, 325-334`
- Modify: `tests/cli.rs`
- Modify: `package.json:20-25`

**Step 1: Write the failing init-output test**

Add a test that runs:

```console
portzilla init skill
```

and asserts stdout byte-for-byte equals `skills/portzilla/SKILL.md`.

**Step 2: Run the focused test and verify it fails**

Run: `cargo test --test cli init_skill`

Expected: clap rejects `skill` as an `init` target.

**Step 3: Write the skill source**

Create `skills/portzilla/SKILL.md` with valid skill frontmatter and concise instructions to:

- Trigger when starting, stopping, checking, or resolving a local development-server port.
- Verify `portzilla` is on `PATH`.
- Inspect the project before choosing a framework-specific command that consumes `PORTZILLA_PORT`.
- Use `portzilla run`, not a pre-claim from an ephemeral shell.
- Report the actual assigned port if reassigned.
- Use `who` and `ls` for inspection and release only an explicitly known lease after checking ownership.
- Direct explicit kill-guard requests to `portzilla init opencode`.

Avoid framework-specific hardcoding and do not tell an agent to write OpenCode configuration implicitly.

**Step 4: Embed and print the exact skill**

Add an `InitHarness::Skill` variant and a `print_init_skill()` helper using `include_str!("../skills/portzilla/SKILL.md")`. It must print only the skill content, allowing this installation command:

```console
mkdir -p .opencode/skills/portzilla
portzilla init skill > .opencode/skills/portzilla/SKILL.md
```

Add the skill asset to npm's `files` list. Cargo compiles the source into the binary, so no runtime asset lookup is needed after a Cargo install.

**Step 5: Run focused tests**

Run: `cargo test --test cli init_skill`

Expected: the emitted and checked-in skill content match exactly.

### Task 5: Document the user workflow and verify packaging

**Files:**
- Modify: `README.md:39-163`
- Modify: `docs/CLI.md:5-177`
- Modify: `docs/GUARD.md:85-98`
- Modify: `package.json:20-25`

**Step 1: Document the happy path**

Add a short README example that uses a command consuming `PORTZILLA_PORT`, such as:

```console
portzilla run 3000 --tag "vite dev" -- sh -c 'npm run dev -- --port "$PORTZILLA_PORT"'
```

State that the exact child command is framework-specific and that Portzilla reports reassignment on stderr while server stdout stays untouched.

**Step 2: Document skill installation**

Document the explicit OpenCode project-skill installation command and explain that `init skill` prints content rather than writing configuration. Keep the existing separate `init opencode` plugin setup for kill guarding.

**Step 3: Update the CLI contract**

Document run arguments, child exit-status propagation, the environment variables, transfer guarantee, reassignment behavior, and cleanup semantics. State clearly that `run` does not support a single-JSON stdout mode.

**Step 4: Validate docs and package contents**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`

Expected: formatting, linting, and the complete suite pass.

Run: `npm pack --dry-run`

Expected: package contents include `skills/portzilla/SKILL.md` and do not include unrelated source or plan files.

Run: `cargo package --allow-dirty --no-verify`

Expected: packaging succeeds and the binary's embedded skill requires no extracted runtime file.

### Task 6: Review the final diff before any commit

**Files:**
- Review: all changed files

**Step 1: Inspect repository state**

Run: `git status --short && git diff --check && git diff --stat`

Expected: only the run command, transfer primitive, tests, skill, and documentation changes are present; no whitespace errors.

**Step 2: Inspect behavior-facing changes**

Run: `git diff -- src/main.rs src/store.rs src/lease.rs tests/cli.rs skills/portzilla/SKILL.md README.md docs/CLI.md docs/GUARD.md package.json`

Expected: no public ownership-transfer command or MCP tool was added, and the skill does not duplicate or weaken the CLI safety guarantees.

**Step 3: Commit only with explicit user approval**

Do not create a commit unless the user explicitly requests it. If approved, stage only the intended implementation files and use a conventional commit message.
