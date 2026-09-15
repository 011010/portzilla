# Session Flight Recorder Design

## Decision

Add a bounded local event journal to the existing lease store. State format v3
will persist current leases and the latest 10,000 audit events in the same JSON
document, under the same lock and atomic rename. This makes every successful
lease mutation and its audit event one consistency boundary without adding a
daemon, database, native dependency, or second-file recovery protocol.

The journal records lease mutations, managed-process exits, and guard deny/warn
decisions. It deliberately does not record guard allows or raw shell commands.
Humans query it through `portzilla history`; agents receive the same read-only
query through MCP. Clearing history remains CLI-only.

## Goals

- Answer which session started, transferred, released, or lost a local server.
- Show when a managed `portzilla run` child exited, separately from later lease
  removal.
- Preserve guard deny/warn evidence without retaining benign command traffic or
  command-line secrets.
- Guarantee that a successful lease mutation cannot be persisted without its
  corresponding event.
- Remain daemon-less, local, cross-platform, and compatible with the current
  owner-only data-directory model.
- Bound storage and query work deterministically.

## Non-goals

- Remote storage, telemetry, synchronization, or cross-machine coordination.
- A daemon or a guarantee that a wrapper terminated by `SIGKILL` records the
  child's exit.
- Event sourcing or replaying the journal to reconstruct current leases.
- Cryptographic signatures, tamper evidence, or compliance-log guarantees.
- Raw command capture, guard-allow events, free-text search, exporters, or a
  TUI.
- Downgrading an already-written v3 state file with an older Portzilla binary.

This is an operational history owned by the local user, not an immutable
security ledger.

## Storage And Migration

`leases.json` advances from format v2 to format v3:

```json
{
  "format_version": 3,
  "next_event_sequence": 42,
  "leases": [],
  "events": []
}
```

The Store will read three shapes:

| Input | In-memory interpretation |
|-------|--------------------------|
| Legacy top-level lease array | Existing leases, empty events, next sequence 1 |
| Versioned v2 object | Existing leases, empty events, next sequence 1 |
| Versioned v3 object | Validate and use all fields |

Read-only operations never migrate files. The first successful mutation or
observational event write persists v3. Older binaries already reject unknown
format versions, so they fail safely instead of discarding journal fields.

The v3 reader validates:

- Lease ports remain unique.
- Event sequences are strictly increasing and unique.
- `next_event_sequence` is greater than every retained sequence and is never
  zero.
- Every event variant and bounded persisted string deserializes correctly.

Invalid state returns an error and is never modified.

## Atomic Update Boundary

The current Store methods independently lock, read leases, mutate, and write.
They will converge on one private state-update helper:

1. Acquire `leases.json.lock` exclusively.
2. Read and validate the complete state.
3. Run one in-memory mutation that returns its result and zero or more event
   drafts.
4. Assign sequence numbers and one operation timestamp while the lock is held.
5. Append the finalized events and trim the oldest excess above 10,000.
6. Serialize leases and events to the temporary file, harden it to `0600`, and
   atomically rename it over `leases.json`.
7. Return success only after the rename succeeds.

`claim`, `transfer`, `release`, and `prune` must use this helper. A serialization,
write, permission, or rename failure leaves the previous state visible and the
operation returns an error. A prune cycle may produce several `lease_pruned`
events, but all removals and events are committed in one write.

Observation-only events use the same lock, validation, sequence assignment,
retention, and write path without changing `leases`.

## Event Model

The persisted envelope is stable and ordered independently of timestamp
resolution:

```text
AuditEvent
  sequence: u64
  occurred_at: u64
  actor: AuditActor
  event: AuditEventKind
  data: variant-specific payload

AuditActor
  source: cli | mcp | run | watch | guard
  harness: opencode | claude_code | cursor | gemini | codex | kimi |
           windsurf | generic | null
  session: string | null
```

The Rust event enum should use Serde's tagged representation with `event` as the
discriminator and `data` as its payload. Persist structured reason codes and
targets; render explanatory prose only at query time.

### Event Kinds

| Event | Required data |
|-------|---------------|
| `lease_claimed` | requested port, claim disposition, resulting lease, optional prior lease |
| `lease_transferred` | wrapper identity and resulting child-owned lease |
| `lease_released` | removed lease and `was_alive` |
| `lease_pruned` | removed dead lease |
| `process_exited` | transferred lease snapshot and tagged exit outcome |
| `guard_denied` | structured target, `foreign_live_lease` reason, affected lease |
| `guard_warned` | structured target and `unresolvable_process_name` reason |
| `history_cleared` | number of prior events removed |

Claim disposition is one of:

```text
created
updated
replaced_dead
reassigned_lease_conflict
reassigned_os_occupied
```

Reassignment is metadata on `lease_claimed`, not a second event. A prior lease
snapshot explains idempotent updates and dead-owner replacement without adding
ambiguous synthetic release/prune events.

The process exit payload is a tagged value: `code` with an integer status,
`signal` with a Unix signal number, or `unknown` where the platform cannot
provide either. Historical lease snapshots are facts captured at event time;
queries never recalculate their liveness.

Guard targets are structured as a PID, port, or bounded process name. For a
multi-target command, a deny records the specific protected target that caused
the decision. No event schema contains a raw-command field.

## Actor Attribution

Actor identity and affected lease ownership are separate. A guard event may be
performed by session A against a lease owned by session B; both facts must
survive and remain visibly distinct.

Actor resolution follows these rules:

| Path | Source and session |
|------|--------------------|
| CLI `claim` | `cli`; explicit lease session, then known ambient session |
| CLI `run` / transfer / exit | `run`; explicit lease session, then known ambient session |
| CLI `release`, `prune`, `history clear` | `cli`; known ambient session or null |
| MCP `claim` | `mcp`; claim session |
| MCP `release` / `prune` | `mcp`; optional `actor_session` parameter |
| Watch cycle | `watch`; null session |
| Hook adapters | `guard`; harness and session from the parsed hook payload |
| Universal guard | `guard`; `generic` harness and resolved guard session |

Known ambient CLI identity resolves non-empty `PORTZILLA_SESSION` first, then
non-empty `CLAUDE_CODE_SESSION_ID`. This attribution does not change lease
ownership or authorization semantics. Existing explicit session behavior stays
authoritative.

## Capture Points

Store owns persistence. CLI, MCP, run, watcher, and guard code provide actor and
operation context but never append JSON themselves.

### Lease Mutations

- `Store::claim` emits exactly one `lease_claimed` event for each successful
  invocation and exposes enough internal disposition data to describe its
  branch accurately.
- `Store::transfer` emits `lease_transferred` in the same write that replaces
  wrapper identity with verified child identity.
- `Store::release` emits `lease_released` only when a lease existed and was
  removed.
- `Store::prune` emits one `lease_pruned` event per removed lease. An empty prune
  does not add noise.

### Managed Process Exit

After `run` has transferred the lease and reaped its child, it appends
`process_exited` before propagating the child's status. Failure to record this
observation prints a warning to stderr but never changes the child's exit code.
The event uses the transferred lease snapshot retained by the wrapper, even if
another process explicitly released or replaced the current lease meanwhile.

If the wrapper itself is terminated before recording, no exit event is
possible. A later prune still records removal of the dead lease.

### Guard Decisions

The guard core returns structured audit evidence alongside `Allow`, `Deny`, or
`Warn`. Adapters serialize only their harness-specific verdict contract; the
runner persists evidence for deny/warn after evaluation. Allow returns no audit
draft.

An event-write failure never reverses or weakens a guard decision. Deny remains
deny, warn remains warn, and structured stdout protocols remain untouched.
Diagnostics use the adapter's existing safe stderr path only.

## Retention And Clearing

The journal retains the latest 10,000 events globally. Every append removes the
oldest excess before writing. Sequence numbers are never reused, so gaps make
retention visible without another metadata field.

`portzilla history clear` atomically removes prior events and appends one
`history_cleared` event with the removed count. It preserves and increments
`next_event_sequence`; even an empty clear records that the command occurred.
MCP intentionally has no clearing tool.

## Query Interfaces

### CLI

```console
portzilla history
portzilla history --session <SESSION>
portzilla history --port <PORT>
portzilla history --event <TYPE>
portzilla history --before <SEQUENCE>
portzilla history --limit <N>
portzilla history --json
portzilla history clear
```

Query behavior:

- Newest event first.
- Default limit 100; accepted range 1 through 1,000.
- Filters combine with logical AND.
- Session matches either `actor.session` or the affected lease's session. The
  result preserves both roles so this convenience cannot hide who did what.
- `before` is an exclusive sequence cursor.
- Fetch one extra matching event to calculate `has_more` without scanning or
  exposing more results than requested.

JSON returns a stable envelope:

```json
{
  "events": [],
  "has_more": false,
  "next_before": null
}
```

When more results exist, `next_before` is the sequence of the last returned
event; the next request uses the strict-less-than `before` rule. Human output
shows sequence, age, kind, source/harness, actor session, affected lease
session, port, PID, and a sanitized summary.

### MCP

Add a read-only `history(session?, port?, event?, before?, limit?)` tool with the
same filtering, ordering, limits, and JSON envelope. Store access stays inside
`spawn_blocking`, matching existing MCP lease tools. Add optional
`actor_session` only to MCP mutation tools that currently cannot attribute the
caller (`release` and `prune`).

## Privacy And Bounds

- Keep the existing `0700` data directory and `0600` state file permissions.
- Never persist raw commands or rendered command strings.
- Persist guard reason enums and structured targets, then generate current
  human text on read.
- Reuse the 512-character session bound and 1,024-character lease-tag bound.
- Add a strict bound for persisted process-name targets and sanitize all human
  rendering with the existing display helpers.
- Invalid observational metadata omits that event with a diagnostic; it never
  turns a deny into allow or changes a child status.
- Invalid mutation metadata rejects the complete mutation before any write.

## Error Semantics

| Failure | Result |
|---------|--------|
| Read/validate v3 before mutation | Mutation fails; file unchanged |
| Build or sequence event for mutation | Mutation fails; file unchanged |
| Serialize/write/rename mutation + event | Mutation fails; previous file remains authoritative |
| Record `process_exited` | Warn on stderr; preserve child status |
| Record guard deny/warn | Preserve verdict/protocol; diagnostic on safe stderr channel |
| Read history | Return normal CLI/MCP store error; never repair implicitly |
| Clear history write | Clear fails; old history remains |

Sequence allocation uses checked arithmetic. Exhausting `u64` rejects the
operation rather than wrapping or reusing identifiers.

## Testing Strategy

### State And Domain Tests

- Decode legacy arrays and v2 objects into empty-history in-memory state.
- Prove read-only list/who/history do not migrate a legacy file.
- Round-trip every v3 event variant.
- Reject duplicate, unordered, zero, or out-of-range sequence metadata.
- Prove trim-at-10,000 and monotonic sequence behavior after retention and
  clear.
- Verify claim dispositions and previous-lease snapshots for every claim path.

### Store Tests

- Assert each successful mutation changes leases and appends the expected
  event in one persisted state.
- Inject write/rename failure and assert neither side of the mutation appears.
- Prove empty release/prune operations do not emit mutation events.
- Run concurrent mutation workers and assert unique ordered sequences, no lost
  events, valid leases, and enforced retention.
- Test all filters, actor-or-owner session matching, exclusive cursor behavior,
  and `has_more` calculation.

### CLI And Lifecycle Tests

- Cover human and JSON history output, combined filters, limits, invalid event
  names, cursors, and clear.
- Run a managed child and assert ordered claim, transfer, and exit events with
  the same actor and lease identity.
- Cover normal exit and Unix signal exit without changing existing status
  propagation.
- Preserve all existing lease, anti-leak, reassignment, and packaging tests.

### Guard And MCP Tests

- Prove deny and warn emit structured events while allow emits none.
- Put a secret sentinel in a raw command and prove it never appears in
  `leases.json` or history output.
- Inject journal failure and verify every adapter retains its established
  stdout, stderr, exit-code, and fail-open/fail-closed contract.
- Cover MCP history schema, pagination, filters, optional mutation actor
  attribution, blocking execution, and absence of a clear tool.

Run formatting, Clippy with warnings denied, all targets/features tests, Cargo
packaging verification, and npm packaging verification. Existing CI provides
native Linux, macOS, and Windows coverage.

## Alternatives Rejected

### SQLite

SQLite provides efficient filtering and multi-record transactions, but adds a
native or bundled dependency, cross-compilation work, migration tooling, and a
larger operational surface. A bounded 10,000-event local journal does not
justify that cost.

### Separate JSONL Journal

Appending JSONL is cheap and naturally log-shaped, but a separate journal and
lease snapshot cannot commit atomically through one filesystem rename. A
write-ahead protocol, transaction markers, crash recovery, and compaction would
be required to uphold the chosen no-gap mutation guarantee.

### Event Log As Source Of Truth

Replaying events could derive current leases from one append-only file, but it
would replace the mature snapshot semantics, complicate legacy migration and
retention, and make corruption recovery more consequential. The flight recorder
is an audit companion to current state, not its source.

## Acceptance Criteria

- Every successful claim, transfer, release, and non-empty prune persists the
  matching event atomically with lease state.
- History never exceeds 10,000 events and sequence identifiers never repeat.
- `run` records observable child exits without changing exit propagation.
- Guard stores deny/warn evidence but no allow event or raw command.
- CLI and MCP return equivalent filtered, paginated history.
- Legacy and v2 users migrate on first write without losing leases.
- Existing Portzilla commands, guard contracts, packaging, and cross-platform
  tests remain green.
