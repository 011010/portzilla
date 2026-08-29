# portzilla — Roadmap

Status legend: **Implemented** (shipped in the current codebase) / **Planned** (not yet built).

## v0.1 — Local lease registry — Implemented

The current release. A daemon-less CLI that tracks port ownership in a locked local JSON file, with an optional foreground watcher for repeated liveness-based pruning.

| Item | Status |
|------|--------|
| `claim` — claim a port with owner PID + tag, forward reassignment on conflict | Implemented |
| `ls` — list all leases | Implemented |
| `who` — show the lease on one port | Implemented |
| `release` — remove a lease | Implemented |
| `prune` — remove all leases whose owning PID is dead | Implemented |
| `watch` — optional foreground watcher with configurable interval and Ctrl-C shutdown | Implemented |
| `--json` on every command | Implemented |
| `PORTZILLA_DATA_DIR` override, `$XDG_DATA_HOME`/`$HOME` fallback | Implemented |
| Atomic, file-locked JSON state (`leases.json` + `leases.json.lock`) | Implemented |
| PID liveness via `sysinfo` process-table lookup | Implemented |
| Exit codes: `0` success, `1` error, `2` lease not found | Implemented |

**Rationale**: ship the smallest possible tool that solves the core coordination problem — declare, query, release, prune — without requiring a central daemon. A CLI that only touches a local file is trivially safe to adopt: no daemon to trust, no network port of its own, no background resource usage. The optional `watch` command adds repeated foreground sweeps for users who want automatic cleanup while preserving that daemon-less default. This is the "small sharp tool" bar, in the spirit of `ripgrep` or `witr`: do one thing, do it with a stable JSON contract, and let other tools (including agents) build on top of it.

**Known limitation carried forward**: new claims record `process_start_time` when the checker resolves it and require both PID and start time to match for liveness. Claims from a checker that cannot resolve identity are treated as unverified and not alive; legacy JSON without the identity marker retains PID-only fallback. `sysinfo` reports start times at one-second resolution, so PID reuse within the same second cannot be distinguished perfectly. The implemented `watch` command repeats point-in-time checks in the foreground; it is not an active process monitor. See the [README limitations section](../README.md#limitations). A future active daemon remains optional and is the stronger supervision mechanism described below.

## v0.1.x — MCP server — Implemented

`portzilla serve --mcp` runs an MCP server over stdio (built on the `rmcp` SDK), exposing `claim`/`who`/`ls`/`release`/`prune` as MCP tools with the same semantics as their CLI counterparts.

| Item | Status |
|------|--------|
| `serve --mcp` subcommand (stdio transport) | Implemented |
| `claim`/`who`/`ls`/`release`/`prune` MCP tools, same flat JSON shapes as CLI `--json` | Implemented |
| Tool descriptions written to steer agent behavior (`claim`: "use this INSTEAD of killing...") | Implemented |
| `claim`'s `pid` defaults to the server's own PID when omitted, with a `note` field flagging it | Implemented |
| Missing-lease results as tool-level errors (`isError: true`), not JSON-RPC protocol errors | Implemented |
| Reads/writes the same locked `leases.json` as the CLI (`PORTZILLA_DATA_DIR` respected) | Implemented |

**Rationale**: the CLI's biggest adoption risk is that agents default to shelling out to `lsof`/`kill` because that's what's in their training and prompts, not because `portzilla` is hard to use. An MCP server removes the "shell out to a subprocess and parse text" friction entirely — an agent with MCP tool access calls `who` the same way it calls any other structured tool, with typed JSON in and out. This is the first and lowest-effort step toward the core goal: making agents actually query ownership before acting, not just making the capability available to them.

## v0.2 — Kill guard core, Claude Code hook — Implemented

A harness-agnostic kill-guard core (`src/guard.rs`) plus a Claude Code adapter (`portzilla hook claude-code`, `portzilla init claude-code`) that intercepts kill-shaped commands before they run and denies the ones that would kill another session's live, leased process.

| Item | Status |
|------|--------|
| `src/guard.rs` — harness-agnostic core: command string + lease list in, `Allow`/`Deny`/`Warn` verdict out | Implemented |
| Pattern detection: `kill`/`kill -9 <pid>`, `pkill`/`killall <name>`, `lsof -ti:<port> \| xargs kill` (+ `-ti :<port>` variant, piped-only correlation), `fuser -k <port>/tcp` (`/udp` too, protocol suffix required), `npx [-y\|--yes] kill-port <port>`/`kill-port <port>` | Implemented |
| Detection also handles absolute/relative paths (`/usr/bin/kill`, matched by basename) and leading env-var assignments (`FOO=1 kill ...`) | Implemented |
| Ownership by session: a lease is "yours" if the caller's session matches the lease's `session`, OR the caller's PID matches the lease's `pid` (either is sufficient) | Implemented |
| `Deny` explanation names the port/pid/tag and says what to do instead (`who`, claim elsewhere, ask the human) | Implemented |
| `Warn` for unresolvable targets (kill-by-process-name) — portzilla can't verify safety, so it says so rather than silently allowing or denying | Implemented |
| `portzilla hook claude-code` — Claude Code `PreToolUse` adapter over the core, schema verified against current docs | Implemented |
| `portzilla init claude-code` — prints (never writes) the `settings.json` registration snippet | Implemented |
| Fail-open at every step: unreadable stdin, malformed hook JSON, unreadable lease store, even an internal panic → allow, never block or crash | Implemented |

**Rationale**: this is identified as the key adoption feature. MCP tool access (v0.1.x) makes querying *possible*; a hook makes the safe path the *default* path by intervening at the exact moment an agent is about to do the destructive thing it would otherwise do unprompted. This directly targets the adoption risk described in the PRD: value only accrues if agents check before killing, and a hook is the mechanism most likely to make that happen without relying on every agent's system prompt independently deciding to use `portzilla`.

**Design decision — harness-agnostic core, thin adapter**: the roadmap beyond this release is "Claude Code first, then any harness." All detection and lease-resolution logic lives in `guard::check`, a pure function that has never heard of Claude Code, hooks, or JSON-RPC. Each adapter translates its harness payload into a normalized request for `hook_common::evaluate`, which delegates to the harness-agnostic `guard::check`, then translates the `Verdict` back into that harness's response shape. A second harness means writing a second thin adapter module against the same evaluation path, not touching the detection logic itself — proven out in v0.2.x below.

**Design decision — ownership is session-based, not PID-based**: the first cut of ownership compared the caller's own PID to the lease's PID, but that's structurally unreachable for a `PreToolUse`-style hook — the hook fires *before* the command's process exists, so there is never a real "caller PID" to offer. Ownership was reworked to also accept a session identifier, matched against the lease's `session` field, with PID kept as a fallback for callers that do have one (e.g. a future harness that checks after the fact). See the Kill guard section in the README for exactly which harnesses can supply a session id today.

**Detection is an approximation, not a shell parser**: segments are split on `|`/`;`/`&&`/`||` and each segment's first word is checked — no quoting, subshell, or variable-expansion awareness. This is a deliberate false-negative-over-false-positive tradeoff (a missed kill is invisible; a wrongly blocked legitimate command breaks the user's session) — see `src/guard.rs`'s module doc comment and its boundary tests (`killall-whatever` doesn't false-trigger `killall`; `echo "kill 123"` doesn't false-trigger `kill`) for exactly where the line is drawn.

## v0.2.x — Multi-harness: Cursor, Gemini CLI, universal wrapper — Implemented

Two more hook adapters over the same unchanged `src/guard.rs` core, plus `portzilla guard -- <command...>` for harnesses with no hook mechanism at all.

| Item | Status |
|------|--------|
| `portzilla hook cursor` / `portzilla init cursor` — Cursor `beforeShellExecution` adapter, schema verified against current docs | Implemented |
| `portzilla hook gemini` / `portzilla init gemini` — Gemini CLI `BeforeTool` adapter scoped to `run_shell_command`, schema verified against current docs | Implemented |
| `portzilla guard [--session <S>] -- <command...>` — universal wrapper: denies (exit 2, command not executed), warns then executes, or executes silently; execs directly on Unix so exit codes pass through unmodified | Implemented |
| `portzilla guard`'s `sh -c`-family unwrapping — combined/separate flags (`-lc`, `-x -c`, `--norc -c`), recursive through nested `sh -c` (bounded depth), documented remaining gaps | Implemented |
| `portzilla guard`'s exec-failure exit codes follow POSIX convention (127 command not found, 126 any other exec failure) | Implemented |

**Verification corrected an assumed premise**: Gemini CLI hooks were assumed to live in a standalone `hooks/hooks.json` inside an extension directory; the current reference documents only a `settings.json`-based mechanism (`.gemini/settings.json`, `~/.gemini/settings.json`, or `/etc/gemini-cli/settings.json`), no separate hooks file format. `portzilla init gemini` was built against the verified mechanism, not the assumed one.

**Own-lease recognition is harness-dependent, and that's now documented rather than assumed away**: verification found no environment variable exposing a session/conversation identifier to the actual shell commands Cursor's or Gemini CLI's agent executes (as opposed to the hook script's own process, which does get one) — `reason` (Gemini, deny only) and `agent_message` (Cursor) still protect against killing a *foreign* session's process on both, but neither can yet recognize a claim as the *current* session's own. Claude Code is the exception: `CLAUDE_CODE_SESSION_ID` is documented as matching the hook payload's `session_id` for Bash subprocesses specifically. See the README's support matrix for the full picture, including `portzilla guard`, which always supports both `--session` and `PORTZILLA_SESSION` regardless of harness.

**Cursor's `agent_message`-on-`allow` (used for `Verdict::Warn`) is a reasoned inference, not a confirmed fact**: the docs' `beforeShellExecution` schema block doesn't carry the "when denied" qualifier some other Cursor hook events' docs do, which is the basis for using it on a `permission: "allow"` response — but absence of a deny-only qualifier is not the same as positive confirmation of delivery on allow. Harmless if wrong (worst case the warning is silently dropped, not a safety regression), but still an open item: a live Cursor smoke test to confirm delivery has not been done.

**A fresh-context review found the first cut of the universal wrapper's `sh -c` unwrap had a real detection bypass**: the original implementation only matched the exact three-argv-element shape `[shell, "-c", payload]`, so `sh -lc '<kill>'`, `sh -x -c '<kill>'`, `bash --norc -c '<kill>'`, and nested `sh -c "sh -c '<kill>'"` all silently bypassed detection and would have executed a kill that should have been denied. Generalized to: any leading run of dash-prefixed flags (combined short clusters like `-lc`/`-eic`, or separate flags with a long flag like `--norc` correctly NOT mistaken for `-c`) before the payload, unwrapped recursively up to 8 levels of nesting via a minimal quote-aware tokenizer. This is still a targeted `sh -c`-family unwrap, not a shell parser — real, disclosed gaps remain (only `sh`/`bash`/`zsh`/`dash` recognized; no command substitution, variable expansion, or escape handling, so `sh -c "$(echo kill 1234)"` is not detected; a flag that takes its own separate argument other than the payload, e.g. `sh -o pipefail -c '...'`, is not modeled) — see `src/guard_cmd.rs`'s module doc comment for the complete list. "Fixed entirely" was the wrong way to describe this the first time it was written here; it is a substantially wider net with known, bounded, disclosed remaining gaps, in the same spirit as `src/guard.rs`'s own documented approximation.

## v0.2.y — Multi-harness: Codex CLI, Kimi CLI — Implemented

Two more hook adapters over the same unchanged `src/guard.rs` core, both built on wire contracts verified against current live documentation after the v0.2.x release.

| Item | Status |
|------|--------|
| `portzilla hook codex` / `portzilla init codex` — Codex CLI `PreToolUse` adapter, schema verified against developers.openai.com/codex/hooks | Implemented |
| `portzilla hook kimi` / `portzilla init kimi` — Kimi CLI `PreToolUse` adapter (exit-code driven), schema verified against the Kimi CLI hooks docs and `src/kimi_cli/hooks/runner.py` | Implemented |

**Verification corrected an outdated premise**: the roadmap previously listed Codex CLI as blocked because "no official, current hooks documentation" existed at the time of writing. That was true then and is false now — Codex hooks reached general availability in May 2026, with a documented `PreToolUse` event whose wire contract (stdin JSON with `session_id`/`tool_name`/`tool_input.command`; stdout JSON `hookSpecificOutput.permissionDecision`/`permissionDecisionReason`; exit 0 + empty stdout = allow) is deliberately close to Claude Code's. `src/codex.rs` is therefore nearly a mirror of `src/claude_code.rs`, with one material difference: Warns ride `additionalContext` (documented as added to model context) rather than being solely a systemMessage concern.

**Codex has a second shell path, `exec_command`, that is documented as observable but not yet positively verified in shape**: the hooks reference says "Unified exec (`exec_command`)" calls are observable by `PreToolUse`, but does not document which matcher string or `tool_input` shape they carry (only "`Bash` and `apply_patch` use `tool_input.command`"). The adapter is scoped to the verified `Bash` tool name only; `exec_command` handling is an open verification item, documented in the module doc rather than assumed.

**Kimi's contract is exit-code driven, not JSON-response driven**: the first-pass assumption was another Claude Code-shaped stdout JSON response, but Kimi's runner (`src/kimi_cli/hooks/runner.py`) implements the docs' simpler contract — exit 0 allows (non-empty stdout is added to the model's context: the model-visible Warn channel), exit 2 blocks (stderr is fed back to the model as a correction). The adapter therefore returns an exit code in its `HookOutcome`, making `run_hook_kimi` the only hook runner whose success path can exit nonzero — the exit code is Kimi's block signal, not an error. A structured JSON deny is also documented, but the exit-2 path was chosen deliberately: the exit code is the unambiguous channel, with no JSON parse between portzilla's verdict and Kimi's runner.

**Own-lease recognition is again absent, for the same verified reason**: neither Codex's nor Kimi's environment-variable references expose a session id to the agent's own shell commands (only to the hook payload). Claude Code remains the only harness with end-to-end own-lease recognition via `CLAUDE_CODE_SESSION_ID`.

**Kimi's hooks are Beta and its project is transitioning**: the hooks docs carry a Beta banner ("implementation details and configuration definitions may change"), and Kimi CLI is being wound down in favor of Kimi Code CLI. The adapter is built against the currently documented contract; `portzilla init kimi` prints a note to re-verify when adopting the successor.

## v0.2.z — Multi-harness: OpenCode, Windsurf — Implemented

Two more adapters over the same unchanged `src/guard.rs` core, both built on wire contracts verified against current live documentation after the v0.2.y release. One (OpenCode) required a structural first; the other (Windsurf) is the simplest adapter in the fleet.

### OpenCode

An OpenCode adapter over the same unchanged `src/guard.rs` core, built on a wire contract verified against the current OpenCode plugins hooks reference before implementing. This one required a structural first: OpenCode hooks are in-process JS/TS plugin modules, not external processes invoked with a JSON payload on stdin, so `portzilla` cannot run as the hook directly the way it does for Claude Code/Codex/Kimi.

| Item | Status |
|------|--------|
| `src/opencode.rs` — binary-side verdict protocol (`portzilla hook opencode`): stdin JSON `{ "session_id", "command" }`, stdout JSON `{ "action": allow/deny/warn, "reason" }`, always exit 0 (the shim reads the verdict from stdout) | Implemented |
| `portzilla init opencode` — prints the full source of the `portzilla.js` plugin shim (a `tool.execute.before` hook that shells out to `portzilla hook opencode`, plus a `shell.env` hook and a `tool.execute.after` hook), with save/restart instructions | Implemented |
| `tests/cli.rs` e2e coverage: the full CLI test-suite pattern red on real `portzilla` binaries — a `portzilla.js` shim-smoke harness driving the shim and the binary's hook protocol both pass / both deny | Implemented |

**The shim is the adapter's second half**: the deny path throws with the reason, which OpenCode surfaces to the model as a tool error; the warn path defers to `tool.execute.after`, appending to the tool result so the model sees it without anything being blocked. The shim enforces its own subprocess timeout (5000 ms) because OpenCode's plugin hooks have no timeout of their own.

**Own-lease recognition finally works outside Claude Code**: the shim's `shell.env` hook injects `PORTZILLA_SESSION` into every bash subprocess the agent runs (verified: bash subprocess env is `{ ...process.env, ...extra.env }`), while `tool.execute.before` gives the shim the session id to pass along — so a claim made with `--session "$PORTZILLA_SESSION"` is recognized as the agent's own by the guard. This is the unique non-Claude harness where end-to-end own-lease recognition resolves.

**Warn delivery is deliberately post-execution**: `tool.execute.before` is binary (return = allow, throw = deny) with no non-blocking model-visible channel; the post-hoc `tool.execute.after` append is the documented tradeoff.

### Windsurf

| Item | Status |
|------|--------|
| `src/windsurf.rs` — Windsurf `pre_run_command` adaptador (`portzilla hook windsurf`): stdin JSON `{ trajectory_id, tool_info.command_line }` (payload shape verified against docs.windsurf.com/windsurf/cascade/hooks), verdict via exit code only (0 allow, 2 block + stderr, any other exit = allow) | Implemented |
| `portzilla init windsurf` — prints the `.windsurf/hooks.json` snippet (workspace-level `pre_run_command` hook) plus alternative user-level paths and the Restricted Mode caveat | Implemented |
| e2e + in-process coverage mirroring the Kimi adapter (twice the same exit-code contract), plus the no-model-visible-warn path (stderr on exit 0) | Implemented |

**Windsurf is the simplest contract in the fleet**: no stdout-JSON response protocol at all — exit 2 + stderr is the block channel (the Cascade agent sees the stderr message), exit 0 allows, and Windsurf documents every other exit code as allow. The adapter is nearly a mirror of `src/kimi.rs`, down to the shared `HookOutcome` shape, differing only in input field names and the warn channel.

**No model-visible warn channel**: `show_output: true` only prints hook stdout/stderr to the user-facing Cascade UI — never to the model. `Verdict::Warn` therefore rides stderr on exit 0: it never blocks, and a human watching Cascade sees it. Same documented tradeoff as Gemini CLI's adapter.

**Own-lease recognition is again absent, for the same verified reason**: `trajectory_id` (the conversation id) reaches the hook payload, but per the Cascade Hooks docs no environment variable exposes it to the shell commands Cascade itself spawns — so claims made from a Cascade session can't be tagged with the id this hook receives. Foreign-lease protection only.

**Restricted Mode**: Cascade hooks do not load or run while a workspace is open in Restricted Mode — the guard is absent there by Windsurf's own design.

## Later — Planned, not yet scheduled

- **Optional daemon with active lease expiry and stale-lease cleanup.** A background process that watches leased PIDs directly (rather than checking on demand) may attempt prompt lease expiry while it is running. It does not kill orphaned server processes, and it cannot guarantee cleanup if it is stopped or crashes. Kept optional so the core tool remains daemon-less by default — this only activates for users who want stronger supervision.
- **Advanced supervision remains distinct from `watch`.** The implemented watcher is a foreground polling convenience: it does not provide active PID monitoring, central IPC, or daemon lifecycle management. The future daemon above is a separate supervision mechanism with no cleanup guarantee when it is not running.
- **TUI dashboard.** A live view of `ls`-equivalent data for interactive human monitoring of a machine with many concurrent sessions, built once there is enough real usage to know what a human actually wants to see at a glance versus what `ls`/`who` already cover.
- **Session flight-recorder journal.** A log of what each session/agent started and stopped over time (not just current state), to answer "what did agent X do to my ports during this session" retrospectively. Depends on session identifiers (`--session`, already in v0.1, now load-bearing for the kill guard too) being used consistently in practice before the journal format is worth committing to.

These are ordered by dependency and risk, not by priority: the work through v0.2.x is a prerequisite for judging whether the "later" items are worth building at all, since it determines whether `portzilla` reaches enough real agent sessions across enough harnesses to know what's actually needed next.
