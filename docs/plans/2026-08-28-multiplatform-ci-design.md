# Multiplatform CI Design

## Goal

Validate Portzilla on the operating systems it supports before adding new
lease features. The current CI runs only on Ubuntu, while the lease identity
and socket probing code has platform-specific behavior.

## Design

Keep one Linux quality job that runs formatting, Clippy, the complete test
suite, and the RustSec audit. Add a native test job with an operating-system
matrix for Ubuntu, macOS, and Windows. Each matrix entry runs the complete
Cargo test suite with all targets and features, so process and socket behavior
is exercised on the host OS rather than only cross-compiled.

Unix-specific tests remain guarded with `#[cfg(unix)]`. The workflow should
not assume Unix shell commands in the cross-platform test job. Release
workflows remain unchanged because they already build and smoke-test release
artifacts and have a different purpose.

## Alternatives Rejected

- Repeating formatting, Clippy, and auditing on every OS adds cost without
  finding OS-specific runtime issues.
- Cross-compilation with `cargo check` does not exercise runtime behavior such
  as socket probing, process liveness, or path handling.

## Verification

- Validate the workflow YAML structure and matrix configuration.
- Run the existing full test and lint commands locally.
- Confirm the resulting diff changes only CI and this design artifact.
