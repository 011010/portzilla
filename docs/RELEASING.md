# Releasing portzilla

## One-time GitHub configuration

Add an npm automation token as the repository secret `NPM_TOKEN`, then set the repository variable `PORTZILLA_PUBLISH_NPM` to `true`.

Leave the variable unset or set it to `false` while testing release builds. The release workflow will still build and upload all binaries, but it will skip npm publication.

## Release checklist

1. Update the version in `Cargo.toml` and `package.json` to the same value.
2. Confirm that both manifests declare exactly version `0.2.0`:

   ```console
   $ cargo_version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "portzilla") | .version')" && npm_version="$(node -p "require('./package.json').version")" && test "$cargo_version" = "0.2.0" && test "$npm_version" = "0.2.0"
   ```

   The release tag must be exactly `v0.2.0`, the `v`-prefixed form of both manifest versions.

3. Run the local checks:

   ```console
   $ cargo fmt --all -- --check
   $ cargo clippy --all-targets --all-features -- -D warnings
   $ cargo test --all-targets --all-features
   $ cargo publish --dry-run
   $ npm pack --dry-run
   ```

4. Inspect the file lists reported by both dry-runs. They should contain only the expected source, manifest, documentation, and package files. Confirm that they exclude `target/` and other build artifacts, local state, credentials, and release-plan artifacts.

5. After all validation passes, manually publish the crate:

   ```console
   $ cargo publish
   ```

6. After publication succeeds, manually create and push the matching tag:

   ```console
   $ git tag v0.2.0
   $ test "$(git describe --tags --exact-match)" = "v0.2.0"
   $ git push origin main --tags
   ```

7. The release workflow builds and smoke-tests Linux x86_64/ARM64, macOS Intel/Apple Silicon, and Windows x86_64/ARM64. It uploads each archive together with its SHA-256 checksum.
8. If `PORTZILLA_PUBLISH_NPM=true`, the release workflow automatically publishes npm after the release assets succeed, using the required `NPM_TOKEN` repository secret.

## Manual verification

After the workflow completes, verify the published installation paths:

```console
$ cargo install portzilla
$ npm install -g portzilla
$ curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/011010/portzilla/main/scripts/install.sh | sh
$ portzilla --help
```

The `curl` and npm installers reject a release archive with a missing or invalid checksum and fall back to Cargo.
