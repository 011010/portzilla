# Hook Adapter Refactor Design

## Goal

Reduce duplicated kill-guard adapter logic while preserving each harness's
verified input, output, failure, and exit-code contract.

## Design

Add a small internal module for the shared decision flow. It will accept a
normalized command request containing the command, optional session, leases,
PID checker, and fail-closed policy, then return the harness-neutral
`guard::Verdict` or a typed portzilla-side failure. The module will centralize
policy-independent command evaluation and common failure classification, but
will not own JSON parsing, response serialization, stdout/stderr choices, or
exit codes.

Each adapter remains responsible for translating its own payload into the
normalized request and translating the result into its documented wire
contract. Claude/Codex similarities will not be forced into a shared external
response type, and Kimi/Windsurf's exit-code contracts remain separate from
JSON adapters. OpenCode's plugin shim remains unchanged.

The refactor must be behavior-preserving. Existing adapter tests remain the
primary contract tests; focused shared-module tests cover allow, deny, warn,
and malformed/missing command handling where the shared module owns those
cases. No new public API is required.

## Alternatives Rejected

- A trait implemented by every adapter would couple unrelated wire contracts
  and make adapter-specific behavior harder to audit.
- A single shared response model would be incorrect for exit-code adapters,
  Gemini's partial response, and OpenCode's verdict protocol.
- Only grouping Claude and Codex would leave duplicated policy flow in the
  remaining adapters and provide little structural improvement.

## Verification

- Compare adapter behavior before and after through the existing unit and
  integration tests.
- Add shared tests for every `guard::Verdict` and failure policy path owned by
  the new module.
- Run formatting, Clippy, all targets/features tests, and diff checks.
