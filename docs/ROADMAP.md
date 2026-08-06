# portzilla — Roadmap

Status legend: **Implemented** (shipped in the current codebase) / **Planned** (not yet built).

## v0.1 — Local lease registry — Implemented

The current release. A daemon-less CLI that tracks port ownership in a locked local JSON file.

| Item | Status |
|------|--------|
| `claim` — claim a port with owner PID + tag, forward reassignment on conflict | Implemented |
| `ls` — list all leases | Implemented |
| `who` — show the lease on one port | Implemented |
| `release` — remove a lease | Implemented |
| `prune` — remove all leases whose owning PID is dead | Implemented |
| `--json` on every command | Implemented |
| `PORTZILLA_DATA_DIR` override, `$XDG_DATA_HOME`/`$HOME` fallback | Implemented |
| Atomic, file-locked JSON state (`leases.json` + `leases.json.lock`) | Implemented |
| PID liveness via `sysinfo` process-table lookup | Implemented |
| Exit codes: `0` success, `1` error, `2` lease not found | Implemented |

**Rationale**: ship the smallest possible tool that solves the core coordination problem — declare, query, release, prune — without any long-running process to install, configure, or keep alive. A CLI that only touches a local file is trivially safe to adopt: no daemon to trust, no network port of its own, no background resource usage. This is the "small sharp tool" bar, in the spirit of `ripgrep` or `witr`: do one thing, do it with a stable JSON contract, and let other tools (including agents) build on top of it.

**Known limitation carried forward**: liveness is a point-in-time PID-table check, not an actively monitored subscription. PID reuse can produce false "alive" reports for a lease whose real owner has already exited. See the [README limitations section](../README.md#limitations). This tradeoff is accepted for v0.1 in exchange for having no daemon; it is revisited in the optional daemon phase below.

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

**Design decision — harness-agnostic core, thin adapter**: the roadmap beyond this release is "Claude Code first, then any harness." All detection and lease-resolution logic lives in `guard::check`, a pure function that has never heard of Claude Code, hooks, or JSON-RPC. `claude_code.rs` only translates a `PreToolUse` payload into a `guard::check` call and translates the `Verdict` back into Claude Code's hook response shape. A second harness means writing a second thin adapter module against the same `guard::check`, not touching the detection logic itself — proven out in v0.2.x below.

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

## Later — Planned, not yet scheduled

- **Adapters for Windsurf, OpenCode, and Codex CLI**, pending verification of each harness's own hook wire contract before implementing (not assumed from any of the three built so far): specifically, whether each exposes a non-blocking, agent-visible feedback channel — Claude Code's `additionalContext` is confirmed; Cursor's `agent_message`-on-`allow` is a reasoned-but-unconfirmed inference (see above); Gemini CLI's `BeforeTool` has no such channel at all — needed for `Verdict::Warn` to actually reach the model rather than only the human, and the exact JSON field carrying a deny reason to the model. Codex CLI in particular has no official, current hooks documentation as of this writing — that adapter additionally depends on such documentation existing at all, not just being verified.
- **Optional daemon with active lease expiry and orphan cleanup.** A background process that watches leased PIDs directly (rather than checking on demand) and can expire or reap leases as soon as their owner exits, closing the PID-reuse gap described in the README's Limitations section. Kept optional so the core tool remains daemon-less by default — this only activates for users who want stronger liveness guarantees.
- **TUI dashboard.** A live view of `ls`-equivalent data for interactive human monitoring of a machine with many concurrent sessions, built once there is enough real usage to know what a human actually wants to see at a glance versus what `ls`/`who` already cover.
- **Session flight-recorder journal.** A log of what each session/agent started and stopped over time (not just current state), to answer "what did agent X do to my ports during this session" retrospectively. Depends on session identifiers (`--session`, already in v0.1, now load-bearing for the kill guard too) being used consistently in practice before the journal format is worth committing to.

These are ordered by dependency and risk, not by priority: the work through v0.2.x is a prerequisite for judging whether the "later" items are worth building at all, since it determines whether `portzilla` reaches enough real agent sessions across enough harnesses to know what's actually needed next.
