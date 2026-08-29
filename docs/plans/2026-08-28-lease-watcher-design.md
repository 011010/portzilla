# Lease Watcher Design

## Goal

Add an optional `portzilla watch` mode that actively removes leases whose
recorded owner processes have exited, without turning Portzilla into a central
IPC service or changing the existing CLI/MCP operations.

## Design

`watch` runs as a long-lived foreground process. At a configurable interval it
opens the existing locked store, checks lease liveness with the current
`SystemPidChecker`, and prunes dead leases. It reuses the store's existing
locking and persistence code, so concurrent `claim`, `release`, `ls`, `who`,
MCP calls, and another watcher remain safe.

The watcher does not listen on a network socket or Unix/Windows IPC endpoint.
Agents continue to use CLI or MCP for claims and queries. The lease remains
associated with the actual server PID supplied to `claim`; if an agent exits
while its child server remains alive, the watcher keeps the lease because the
server is still the recorded owner.

The command runs until interrupted and handles termination cleanly. Its
interval is configurable through a CLI flag with a conservative default. Each
prune cycle reports removed leases in human-readable mode, while JSON mode
emits machine-readable events suitable for an agent supervisor. Store errors
are reported and retried on the next cycle rather than terminating the
watcher, unless startup itself cannot open the data directory.

## Alternatives Rejected

- A central daemon with an IPC protocol would add installation, permissions,
  lifecycle, and recovery complexity without improving the current CLI/MCP
  contract.
- Agent heartbeats would require every harness integration to implement lease
  renewal and would make normal process liveness less authoritative.

## Verification

- Unit-test the watch-cycle behavior with a controllable clock or one-shot
  cycle function and fake PID checker.
- Test that dead leases are removed while live leases remain.
- Test interval validation and clean shutdown behavior.
- Test concurrent store access through the existing lock.
- Run formatting, Clippy, the complete test suite, and workflow checks.
