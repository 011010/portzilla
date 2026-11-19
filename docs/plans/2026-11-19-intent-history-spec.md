# Intent history — spec for the implementer

## Goal

Give file intents the same retrospective the lease store already has: a bounded,
queryable journal of claim and release events, exposed by the CLI and MCP.

## Data model (intents.json, format_version 3)

Envelope becomes:

```json
{"format_version":3,"intents":[...],"events":[...],"next_event_sequence":1}
```

- `format_version` 1 and 2 stay readable; the next write upgrades to 3. Any
  version above 3 is rejected without rewriting (existing rule and message).
- `next_event_sequence` follows the lease store's convention: 1-based, monotonic,
  never reused, and validated on read (zero is invalid).
- `MAX_INTENT_EVENTS = 10_000`. When an insert would exceed it, drop the oldest
  events (same trimming rule as the lease journal) and keep the newest.

Event record:

```json
{"seq":1,"at":1795000000,"event":"claim_created","key":"<opaque key>","session":"auth","previous_session":null}
```

- `event` is one of `claim_created`, `claim_refreshed`, `claim_taken_over`, `released`.
- `previous_session` is present only for `claim_taken_over` (the session whose
  expired claim was replaced).
- `at` is the same `now` the mutation used, so tests can drive it deterministically.

## When each event is recorded (all under the existing exclusive lock, in the same atomic write)

| Operation | Event |
|---|---|
| `claim` on a free key | `claim_created` |
| `claim` refreshing a live claim owned by the same session | `claim_refreshed` |
| `claim` taking over an expired claim from another session | `claim_taken_over` (with `previous_session`) |
| `claim` taking over an expired claim from the same session | `claim_refreshed` (no takeover of a foreign holder) |
| `release` by the owner | `released` |
| `release` of an absent key, capacity refusal, validation error, read | no event |

Expired records reaped as a side effect of `claim` are **not** logged: they are
housekeeping, not decisions, and logging them would flood the journal.

## Store API additions

```rust
pub struct IntentEvent { pub seq: u64, pub at: u64, pub event: IntentEventKind,
                         pub key: String, pub session: String,
                         pub previous_session: Option<String> }

pub enum IntentEventKind { ClaimCreated, ClaimRefreshed, ClaimTakenOver, Released }

pub struct IntentHistoryQuery { session, event, before, limit }  // mirrors audit::HistoryQuery
pub struct IntentHistoryPage { events: Vec<IntentEvent>, next_before: Option<u64> }

IntentStore::history(&self, query: &IntentHistoryQuery) -> Result<IntentHistoryPage>
IntentStore::clear_history(&self) -> Result<usize>
```

Validation mirrors `audit::HistoryQuery::try_new`: `limit` defaults to 100 and is
capped (reuse the lease constant if it is public), `before` is an exclusive
cursor, `session` and `event` are length-bounded.

## CLI

`portzilla intent history [--session S] [--event E] [--limit N] [--before SEQ] [--json]`
and `portzilla intent history clear`.

- Human table: `SEQ AT EVENT SESSION KEY` (sanitize free text with
  `sanitize_for_display`).
- JSON: the page shape, same as the lease history page (`events`, `next_before`).
- Exit codes: 0 on success (including empty), 1 on validation/store error.
- `--event` accepts the four event names; an unknown name is a validation error
  naming the accepted values.

## MCP

New read-only tool `intent_history` with the same filters and page shape.
Annotations: `read_only_hint = true`, `destructive_hint = false`,
`idempotent_hint = true`, `open_world_hint = false`. Update the wire-level tool
list test. `clear` stays CLI-only, matching the lease history rule.

## Tests

- Each event kind is recorded by the operation that produces it (including
  takeover carrying `previous_session`).
- No event for: read, absent release, capacity refusal, invalid input.
- Filters compose: session, event, `before` as an exclusive cursor, limit.
- Sequence is monotonic and never reused after trimming.
- Trimming: seed `MAX_INTENT_EVENTS + 1` events (call the store directly), assert
  the oldest is gone and the newest is present.
- v2 file upgrades to v3 on the next write, preserving intents and starting the
  sequence at 1.
- Unknown version (99) is rejected and the file is untouched.
- `clear_history` removes events and leaves intents intact, returning the count.
- CLI: table and JSON shapes, `--event` validation, `history clear`.
- MCP: tool registered, page shape, filter passthrough.

## Constraints

- No new dependencies.
- Do not change the lease journal or `audit.rs`.
- Keep the atomic single-write invariant: state and events change together or not
  at all.
- Never stage `.gitignore`, `docs/CLI.md`, `.atl/`, `.playwright-mcp/`,
  `graphify-out/`.
