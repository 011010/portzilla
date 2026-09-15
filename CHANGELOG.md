# Changelog

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
