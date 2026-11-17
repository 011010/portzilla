# Changelog

## Unreleased

- Added file-intent leases: the OpenCode shim denies a `write`/`edit` on a file another live session is editing, keyed by repository plus relative path so two worktrees of one repository collide while unrelated repositories do not.
- Rewrote the OpenCode plugin shim for the V2 plugin API. OpenCode 2.3.0 rejects the V1 shape, so the previous shim did not load.
- Dropped the OpenCode `shell.env` / `PORTZILLA_SESSION` injection: V2 has no equivalent, and it never applied to the file-intent path, which uses the session id the edit hook already provides.
- Made the test suite deterministic: dedicated non-ephemeral test-port ranges, per-test serialization, and a probe that no longer blocks its own loopback check.

## 0.4.0

- Added a bounded session flight recorder with structured audit events for lease claims, releases, transfers, pruning, and guard decisions.
- Added `portzilla history` with JSON output, filtering, pagination, and history clearing.
- Added MCP tools for lease history and structured process/port ownership queries.
- Added actor and session attribution across CLI commands, hooks, the guard wrapper, and MCP operations.
- Added community health files, issue forms, and release documentation for the project.

## 0.3.0

- Added `portzilla run` to claim a port, launch a server with the actual port in `PORTZILLA_PORT`, and transfer the lease to the verified child process.
- Added `PORTZILLA_SESSION` propagation and child exit-status forwarding for managed launches.
- Added an installable agent skill, available from `portzilla init skill`, that teaches agents to use the safe server lifecycle.
- Added bounded audit history with `portzilla history`, MCP read-only queries, lease/process lifecycle events, and structured guard deny/warn evidence.
- Added best-effort journal persistence to all hook adapters and the universal `portzilla guard` wrapper without changing their wire contracts.
- Made the release-persistence test independent of ephemeral port allocation.

## 0.2.0

- Hardened lease identity checks against PID reuse and stale ownership.
- Added the versioned state envelope (v2); older binaries cannot read v2 state files and must be upgraded before using that data directory.
- Reassignment now accounts for operating-system port occupancy.
- Added multiplatform CI coverage.
- Added optional, foreground `watch` support for lease maintenance.
- Evaluated shared hook-adapter support across integrations.

Limitations: Portzilla does not manage or kill processes. The watcher is optional and runs in the foreground; an active daemon remains future work.
