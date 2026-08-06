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

## v0.2 — Kill guard, Claude Code hook — Implemented

A harness-agnostic kill-guard core (`src/guard.rs`) plus a Claude Code adapter (`portzilla hook claude-code`, `portzilla init claude-code`) that intercepts kill-shaped commands before they run and denies the ones that would kill another session's live, leased process.

| Item | Status |
|------|--------|
| `src/guard.rs` — harness-agnostic core: command string + lease list in, `Allow`/`Deny`/`Warn` verdict out | Implemented |
| Pattern detection: `kill`/`kill -9 <pid>`, `pkill`/`killall <name>`, `lsof -ti:<port> \| xargs kill` (+ `-ti :<port>` variant), `fuser -k <port>/tcp`, `npx kill-port <port>`/`kill-port <port>` | Implemented |
| `Deny` explanation names the port/pid/tag and says what to do instead (`who`, claim elsewhere, ask the human) | Implemented |
| `Warn` for unresolvable targets (kill-by-process-name) — portzilla can't verify safety, so it says so rather than silently allowing or denying | Implemented |
| `portzilla hook claude-code` — Claude Code `PreToolUse` adapter over the core, schema verified against current docs | Implemented |
| `portzilla init claude-code` — prints (never writes) the `settings.json` registration snippet | Implemented |
| Fail-open at every step: unreadable stdin, malformed hook JSON, unreadable lease store, even an internal panic → allow, never block or crash | Implemented |

**Rationale**: this is identified as the key adoption feature. MCP tool access (v0.1.x) makes querying *possible*; a hook makes the safe path the *default* path by intervening at the exact moment an agent is about to do the destructive thing it would otherwise do unprompted. This directly targets the adoption risk described in the PRD: value only accrues if agents check before killing, and a hook is the mechanism most likely to make that happen without relying on every agent's system prompt independently deciding to use `portzilla`.

**Design decision — harness-agnostic core, thin adapter**: the roadmap beyond this release is "Claude Code first, then any harness." All detection and lease-resolution logic lives in `guard::check`, a pure function that has never heard of Claude Code, hooks, or JSON-RPC. `claude_code.rs` only translates a `PreToolUse` payload into a `guard::check` call and translates the `Verdict` back into Claude Code's hook response shape. A second harness (Cursor, a generic shell wrapper, whatever comes next) means writing a second thin adapter module against the same `guard::check`, not touching the detection logic itself.

**Detection is an approximation, not a shell parser**: segments are split on `|`/`;`/`&&`/`||` and each segment's first word is checked — no quoting, subshell, or variable-expansion awareness. This is a deliberate false-negative-over-false-positive tradeoff (a missed kill is invisible; a wrongly blocked legitimate command breaks the user's session) — see `src/guard.rs`'s module doc comment and its boundary tests (`killall-whatever` doesn't false-trigger `killall`; `echo "kill 123"` doesn't false-trigger `kill`) for exactly where the line is drawn.

## v0.2.x — Other harness adapters — Planned

Adapters for harnesses beyond Claude Code (Cursor, a generic pre-exec shell wrapper, etc.), each a thin translation layer over the same `guard::check` core used by the Claude Code adapter.

**Rationale**: the core was built harness-agnostic specifically so this is additive work, not a rewrite — validate the adapter pattern against a second real harness once there's a concrete integration point to design against, rather than speculatively generalizing now.

## Later — Planned, not yet scheduled

Three further directions, listed in order of how directly each extends the coordination model established in v0.1:

- **Optional daemon with active lease expiry and orphan cleanup.** A background process that watches leased PIDs directly (rather than checking on demand) and can expire or reap leases as soon as their owner exits, closing the PID-reuse gap described above. Kept optional so the core tool remains daemon-less by default — this only activates for users who want stronger liveness guarantees.
- **TUI dashboard.** A live view of `ls`-equivalent data for interactive human monitoring of a machine with many concurrent sessions, built once there is enough real usage to know what a human actually wants to see at a glance versus what `ls`/`who` already cover.
- **Session flight-recorder journal.** A log of what each session/agent started and stopped over time (not just current state), to answer "what did agent X do to my ports during this session" retrospectively. Depends on session identifiers (`--session`, already in v0.1) being used consistently in practice before the journal format is worth committing to.

These are ordered by dependency and risk, not by priority: the daemon and adapter work (v0.1.x/v0.2/v0.2.x) are prerequisites for judging whether the "later" items are worth building at all, since they determine whether `portzilla` reaches enough real agent sessions to know what's actually needed next.
