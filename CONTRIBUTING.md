# Contributing to Portzilla

## Before You Start

Search existing issues first. For a new feature or bug, open an issue and wait for `status:approved` before starting a pull request. Keep each pull request focused on one problem.

## Development

Portzilla is a Rust CLI with a small Node.js packaging shim. Use the repository's stable Rust toolchain and run tests serially when working on state or port-allocation behavior:

```console
$ cargo fmt --all
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo test --all-targets --all-features -- --test-threads=1
```

For packaging changes, also run `cargo package --locked` and `npm pack --dry-run`.

## Pull Requests

- Link the approved issue with `Closes #<number>`.
- Explain the behavior change and the tests that prove it.
- Update documentation when a CLI, hook, MCP, or state-file contract changes.
- Preserve existing JSON and exit-code contracts unless the issue explicitly changes them.
- Use conventional commit messages such as `feat(scope): description` or `fix(scope): description`.
- Do not include secrets, generated artifacts, or `Co-Authored-By` trailers.

CI must pass before merging. Maintainers may request changes when the implementation, tests, documentation, or security properties are incomplete.
