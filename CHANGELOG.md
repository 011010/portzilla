# Changelog

## 0.3.0

- Added `portzilla run` to claim a port, launch a server with the actual port in `PORTZILLA_PORT`, and transfer the lease to the verified child process.
- Added `PORTZILLA_SESSION` propagation and child exit-status forwarding for managed launches.
- Added an installable agent skill, available from `portzilla init skill`, that teaches agents to use the safe server lifecycle.
- Made the release-persistence test independent of ephemeral port allocation.

## 0.2.0

- Hardened lease identity checks against PID reuse and stale ownership.
- Added the versioned state envelope (v2); older binaries cannot read v2 state files and must be upgraded before using that data directory.
- Reassignment now accounts for operating-system port occupancy.
- Added multiplatform CI coverage.
- Added optional, foreground `watch` support for lease maintenance.
- Evaluated shared hook-adapter support across integrations.

Limitations: Portzilla does not manage or kill processes. The watcher is optional and runs in the foreground; an active daemon remains future work.
