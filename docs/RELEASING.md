# Releasing portzilla

## One-time GitHub configuration

Add an npm granular access token scoped to the `portzilla` package with Read and write permission and Bypass two-factor authentication enabled as the repository secret `NPM_TOKEN`, and a crates.io token as `CARGO_REGISTRY_TOKEN`. Use a short expiration with rotation; when available prefer npm trusted publishing (OIDC) over a long-lived bypass token, since npm is deprecating direct publishing with bypass tokens. Set the repository variables `PORTZILLA_PUBLISH_NPM` and `PORTZILLA_PUBLISH_CARGO` to `true` when the workflow should publish both packages.

Leave either variable unset or set it to `false` to skip that registry. The release workflow will still build, smoke-test, and upload all binary assets.

## Dependency policy

The dependency-policy step in `ci.yml` (`cargo deny check`) reads a database that changes independently of this repository. A commit that was green yesterday can turn red today with no code change: that is exactly what happened in #15 and again in #19, when three advisories for the same `rmcp` release appeared at once.

The policy is therefore to stay *ahead* of the database rather than react to it:

- **Cargo dependencies** get a weekly grouped Dependabot PR (minor and patch together, major versions separate) so a routine week is one reviewable diff. Security updates are deliberately left ungrouped: they should stand out in the PR list, not hide inside a routine bump.
- **GitHub Actions** are bumped monthly. A stale action is the same supply-chain risk one layer up.
- **Advisories are never ignored in bulk.** If an advisory must be ignored, add it to the audit step with a comment naming the reason and the version that removes the need, and open an issue to track it. An ignore with no expiry is how a real vulnerability gets forgotten.
- **A red dependency check blocks the merge.** Do not `--admin` past it and do not mark the step `continue-on-error`: the failure is the signal that a dependency needs bumping, and the fix is a one-line PR like #20.
- **`deny.toml` is the policy, not a snapshot.** It allows permissive licenses deliberately, reports duplicate versions as warnings, and fails on vulnerabilities and on unmaintained crates **except** the one advisory listed in `ignore`. Every ignore must name an ID, a reason, and what would remove the need for it; the current entry is `RUSTSEC-2026-0543` (`chrono`, reached through `rmcp`).

## Release checklist

1. Update the version in `Cargo.toml` and `package.json` to the same value, then let Cargo update the root package entry in `Cargo.lock`.
2. Confirm that both manifests declare exactly version `0.5.0`:

   ```console
   $ cargo_version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "portzilla") | .version')" && npm_version="$(node -p "require('./package.json').version")" && test "$cargo_version" = "0.5.0" && test "$npm_version" = "0.5.0"
   ```

   The release tag must be exactly `v0.5.0`, the `v`-prefixed form of both manifest versions.

3. Run the local checks:

   ```console
   $ cargo fmt --all -- --check
   $ cargo clippy --all-targets --all-features -- -D warnings
   $ cargo test --all-targets --all-features
   $ cargo publish --dry-run
   $ npm pack --dry-run
   ```

4. Inspect the file lists reported by both dry-runs. They should contain only the expected source, manifest, documentation, and package files. Confirm that they exclude `target/` and other build artifacts, local state, credentials, and release-plan artifacts.

5. Commit the release preparation, merge it into `main`, and wait for `main` CI to pass.
6. Create and push the matching tag from the verified `main` commit:

   ```console
   $ git tag v0.5.0
   $ test "$(git describe --tags --exact-match)" = "v0.5.0"
   $ git push origin v0.5.0
   ```

7. The release workflow builds and smoke-tests Linux x86_64/ARM64, macOS Intel/Apple Silicon, and Windows x86_64/ARM64. It uploads each archive together with its SHA-256 checksum.
8. When their repository variables are enabled, the workflow publishes npm and crates.io only after every release asset succeeds.

## Manual verification

After the workflow completes, verify the published installation paths:

```console
$ cargo install portzilla
$ npm install -g portzilla
$ curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/011010/portzilla/main/scripts/install.sh | sh
$ portzilla --help
$ portzilla run --help
```

The `curl` and npm installers reject a release archive with a missing or invalid checksum and fall back to Cargo.
