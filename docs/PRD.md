# portzilla — Product Requirements Document

## Status

Version 0.2.0 release preparation. The implemented feature set includes the local lease registry (`claim`, `ls`, `who`, `release`, `prune`, `watch`), JSON and locked state, the MCP server, the kill guard with Claude Code, Cursor, Gemini CLI, Codex CLI, Kimi CLI, OpenCode, and Windsurf adapters, and the universal `guard` wrapper. This document describes the product as a whole, including work not yet built — see [`ROADMAP.md`](ROADMAP.md) for what is implemented versus planned.

## Problem statement

Developers increasingly run several AI coding-agent sessions in parallel on the same machine: one per git worktree, one per feature branch, sometimes one per tool (Claude Code in one pane, Cursor in another). Each session typically starts its own dev server, and dev servers default to well-known ports (3000, 5173, 8080, ...).

When an agent tries to start a server and finds the port occupied, its default behavior — encouraged by how most agents are prompted and by the tools available to them (`lsof`, `kill`, `kill-port`) — is to treat the occupying process as garbage and kill it. The agent has no way to tell "this port is busy because it's mine from an earlier step" apart from "this port is busy because a sibling session's dev server is running there." Both look identical from the outside: a PID bound to a port.

The result is destructive interference between sessions that are otherwise fully isolated (separate worktrees, separate branches, separate contexts) except for one shared resource: the localhost port space. A second failure mode compounds this: when a session ends (agent process exits, worktree is torn down, terminal is closed) without explicit cleanup, its dev server can be left running as an orphan, occupying a port indefinitely with nothing tracking that it should be stopped.

Existing tools do not address coordination:

- `lsof -i :PORT`, `kill-port`, ServerSlayer, `witr` — all diagnostic or destructive. They tell you what is on a port, or they kill it. None of them let a process declare "I am using this port for this reason" before a conflict happens, and none of them let another process check that declaration before acting.
- Sandboxes / containers — isolate the port namespace entirely, which solves the conflict by preventing sharing, not by coordinating it. Many dev workflows (see below) need agents sharing the host network namespace, e.g. to reach `localhost` services from the host browser or another tool.

Nobody in this space treats "who currently owns this port and why" as a queryable, declared fact instead of an inference from process state.

## Target users

- **Primary**: developers running multiple AI coding-agent sessions in parallel — worktrees, Claude Code, Cursor, and similar tools — on a single development machine.
- **Secondary**: teams that want to standardize local dev environment conventions (consistent port claiming/tagging across a team, even without agents involved).
- **Constraint**: the tool must be fully usable by a human typing commands in a terminal, with no agent in the loop. Agent integration is additive, not a hard dependency.

## Use cases

1. **Agent claims a port before starting a dev server.** An agent about to run `npm run dev` first runs `portzilla claim 3000 --tag "my-app dev server" --session <session-id>`. If 3000 is free or was held by a dead process, it gets 3000. If a live process already holds 3000 under a different owner, `portzilla` hands back the next free port instead, and the agent starts its server on that port instead of on 3000.
2. **Agent checks ownership before killing.** An agent that hits `EADDRINUSE` on a port runs `portzilla who 3000 --json` before deciding to kill anything. If the lease belongs to a different session's PID with a live process, the agent backs off (or claims a different port) instead of killing a sibling's server.
3. **Human audits leases.** A developer running several worktrees runs `portzilla ls` to see every claimed port, its owning PID, its tag, and its age at a glance, without having to cross-reference `lsof` output against which terminal tab is running what.
4. **Session cleanup.** When a session ends, its process calls `portzilla release <port>` for each port it claimed. If cleanup didn't run (crash, force-kill), a later `portzilla prune` removes every lease whose owning PID is no longer alive, keeping the registry accurate without manual bookkeeping.

## Business rules

- **Never steal a live lease.** A claim on a port already held by a different, live PID is never overwritten in place. The requester is redirected to another port instead.
- **Reassign forward.** When a conflict occurs, the next available port is the first port at or after `requested_port + 1` that has neither a live lease nor an OS-level bind on it. Reassignment always moves forward from the requested port, never to an arbitrary or lower port.
- **Idempotent re-claim.** The same PID re-claiming the same port it already holds is always safe — it updates the lease (tag, session) in place rather than being treated as a conflict or creating a duplicate entry.
- **Explicit release always wins.** `release` removes the lease unconditionally, regardless of whether the owning PID is still alive. It is a declaration of intent by the caller, not a liveness check; if the PID is in fact still alive, `portzilla` warns on stderr but still performs the release. Ownership is a coordination signal, not an access-control mechanism `portzilla` enforces.
- **A dead owner never blocks a claim.** If the PID recorded on a lease is no longer alive, a new claim on that port succeeds directly on the requested port rather than triggering a reassignment.

## Non-goals

- **Not a process manager.** `portzilla` does not start, stop, restart, or supervise the dev servers or other processes bound to leased ports. It only tracks the claim.
- **Not a firewall.** `portzilla` does not restrict network access to a port, and does not prevent any process from binding a port it has not leased.
- **Not a sandbox.** `portzilla` does not isolate processes or network namespaces from each other. Isolation-based solutions (containers, network namespaces) solve the same underlying conflict differently; `portzilla` is for the case where isolation is undesirable or impractical (shared localhost access from host tools/browsers).
- **No network coordination between machines in v0.x.** All state is local to a single machine's data directory. Coordinating leases across multiple machines (e.g. a shared dev server pool) is out of scope for the versions currently planned.

## Success metrics

Because `portzilla` is a small, free CLI tool with no telemetry, success is measured through public adoption signals rather than in-product metrics:

- crates.io download counts after publishing.
- GitHub stars and issue/PR activity as a proxy for organic interest and usage.
- Number of agent tools and workflows integrating `portzilla` via the planned MCP server, and number of Claude Code hook installs — the clearest signal that the "coordination, not diagnostics" value proposition is being realized in actual agent sessions rather than only in manual terminal use.

## Competitive landscape

| Tool | Category | What it does | What it doesn't do |
|------|----------|---------------|---------------------|
| `witr` | Diagnostic | Shows what's listening on a port | No claiming, no ownership record, no coordination |
| `kill-port` | Destructive | Kills whatever is bound to a port | No check for legitimate ownership before killing |
| ServerSlayer | Destructive | Bulk-kills dev server processes | Same as above, at a larger blast radius |
| Sandboxes / containers | Isolation | Give each session its own network namespace | Solves conflict by preventing sharing, not by coordinating within a shared namespace; heavier setup; doesn't help when host-level `localhost` access is required |
| `portzilla` | Coordination | Lets processes declare and query port ownership before acting | Does not itself manage, firewall, or sandbox processes |

The gap `portzilla` fills is the coordination layer that sits before any of the diagnostic or destructive tools would be reached for: a query that answers "should I even consider touching this port?" before `kill-port` or a manual `kill -9` ever runs.

## Risks

- **Adoption depends on agent-side integration.** The core value proposition — agents checking ownership before killing — only holds if agents actually call `portzilla` instead of defaulting to `lsof`/`kill` patterns baked into their training or their tool prompts. A CLI that only humans use manually captures a fraction of the intended value.
  - *Mitigation*: the roadmap prioritizes an MCP server (v0.1.x) so agents with MCP tool access can call `portzilla` natively without shelling out, followed by a Claude Code hook (v0.2) that intercepts kill/`lsof`-style command patterns at the source and suggests a `portzilla who` check — turning the safe path into the path of least resistance instead of relying on agents choosing to adopt a new tool unprompted.
- **PID-reuse false positives** (see [`README.md`](../README.md#limitations)) could erode trust if a stale lease is reported `alive` and blocks a legitimate reclaim. Mitigated short-term by `prune` being cheap and safe to run frequently; longer-term by the optional daemon with active lease expiry on the roadmap.
