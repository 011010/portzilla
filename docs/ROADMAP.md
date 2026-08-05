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

## v0.1.x — MCP server — Planned

Add `portzilla serve --mcp`, exposing `claim`/`ls`/`who`/`release`/`prune` as MCP tools via the `rmcp` SDK.

**Rationale**: the CLI's biggest adoption risk is that agents default to shelling out to `lsof`/`kill` because that's what's in their training and prompts, not because `portzilla` is hard to use. An MCP server removes the "shell out to a subprocess and parse text" friction entirely — an agent with MCP tool access calls `portzilla_who` the same way it calls any other structured tool, with typed JSON in and out. This is the first and lowest-effort step toward the core goal: making agents actually query ownership before acting, not just making the capability available to them.

## v0.2 — Claude Code hook integration — Planned

A Claude Code hook that intercepts commands matching kill/`lsof`-on-a-port patterns (e.g. `kill $(lsof -ti:3000)`, `kill-port 3000`) before they execute, and suggests running `portzilla who <port>` first.

**Rationale**: this is identified as the key adoption feature. MCP tool access (v0.1.x) makes querying *possible*; a hook makes the safe path the *default* path by intervening at the exact moment an agent is about to do the destructive thing it would otherwise do unprompted. This directly targets the adoption risk described in the PRD: value only accrues if agents check before killing, and a hook is the mechanism most likely to make that happen without relying on every agent's system prompt independently deciding to use `portzilla`.

## Later — Planned, not yet scheduled

Three further directions, listed in order of how directly each extends the coordination model established in v0.1:

- **Optional daemon with active lease expiry and orphan cleanup.** A background process that watches leased PIDs directly (rather than checking on demand) and can expire or reap leases as soon as their owner exits, closing the PID-reuse gap described above. Kept optional so the core tool remains daemon-less by default — this only activates for users who want stronger liveness guarantees.
- **TUI dashboard.** A live view of `ls`-equivalent data for interactive human monitoring of a machine with many concurrent sessions, built once there is enough real usage to know what a human actually wants to see at a glance versus what `ls`/`who` already cover.
- **Session flight-recorder journal.** A log of what each session/agent started and stopped over time (not just current state), to answer "what did agent X do to my ports during this session" retrospectively. Depends on session identifiers (`--session`, already in v0.1) being used consistently in practice before the journal format is worth committing to.

These are ordered by dependency and risk, not by priority: the daemon and hook work (v0.1.x/v0.2) are prerequisites for judging whether the "later" items are worth building at all, since they determine whether `portzilla` reaches enough real agent sessions to know what's actually needed next.
