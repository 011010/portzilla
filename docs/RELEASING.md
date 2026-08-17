# Releasing portzilla

## One-time GitHub configuration

Add an npm automation token as the repository secret `NPM_TOKEN`, then set the repository variable `PORTZILLA_PUBLISH_NPM` to `true`.

Leave the variable unset or set it to `false` while testing release builds. The release workflow will still build and upload all binaries, but it will skip npm publication.

## Release checklist

1. Update the version in `Cargo.toml` and `package.json` to the same value.
2. Run the local checks:

   ```console
   $ cargo fmt --all -- --check
   $ cargo clippy --all-targets --all-features -- -D warnings
   $ cargo test --all-targets
   $ cargo publish --dry-run
   $ npm pack --dry-run
   ```

3. Publish the crate:

   ```console
   $ cargo publish
   ```

4. Create and push the matching tag:

   ```console
   $ git tag v0.1.0
   $ git push origin main --tags
   ```

5. The release workflow builds and smoke-tests Linux x86_64/ARM64, macOS Intel/Apple Silicon, and Windows x86_64/ARM64. It uploads each archive together with its SHA-256 checksum.
6. If `PORTZILLA_PUBLISH_NPM=true`, npm is published automatically after every release asset succeeds.

## Manual verification

After the workflow completes, verify the published installation paths:

```console
$ cargo install portzilla
$ npm install -g portzilla
$ curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/011010/portzilla/main/scripts/install.sh | sh
$ portzilla --help
```

The `curl` and npm installers reject a release archive with a missing or invalid checksum and fall back to Cargo.
