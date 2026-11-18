# Lease Identity and Port Occupancy

## Goal

Prevent stale leases from being treated as live after PID reuse, and prevent
`claim` from recording ownership for a port already occupied by an unregistered
process.

## Design

### Process identity

Extend `Lease` with an optional process-start timestamp. New claims capture the
start timestamp for the claimed PID. Liveness requires both the PID to exist and
its current start timestamp to match the recorded value.

The field is optional for backward compatibility with existing `leases.json`
files. A legacy lease without a timestamp keeps the current PID-only behavior
until it is replaced or renewed by a new claim.

The process checker abstraction will expose process identity rather than only a
boolean liveness result. Test checkers will provide deterministic identities.

### Port occupancy

`claim` will probe the requested port even when there is no live lease. If the
OS bind probe reports the port occupied, it will scan forward for the first port
that has neither a live lease nor an OS-level bind.

The claim result will identify why reassignment happened: a live lease conflict
or an unregistered OS occupant. Existing `reassigned` behavior remains intact
for compatibility with current CLI and MCP consumers.

The OS probe remains advisory and has an unavoidable time-of-check/time-of-use
window before the caller starts its server. This iteration does not introduce a
daemon or a physical port reservation.

### Compatibility and failure behavior

- Existing leases without `process_start_time` remain readable.
- Existing JSON fields and command exit codes remain stable.
- CLI and MCP expose the new optional identity/reassignment metadata.
- A missing process start time for a newly requested PID is treated as a dead
  process by the same liveness rules used today.

## Testing

- Unit tests for matching and mismatching process start identities.
- Regression test proving a reused PID is not considered the original owner.
- Unit and CLI tests for an unregistered occupied requested port.
- Regression tests proving live lease conflicts and free-port claims retain
  their existing behavior.
- Full format, Clippy, and test-suite verification.

## Non-goals

- No daemon or background cleanup process.
- No physical reservation held between `claim` and server startup.
- No migration command for old state files.
