# crates.io Publish Design

## Goal

Publish the Cargo package from the release workflow without coupling it to npm credentials.

## Design

Add a `Publish crates.io package` job after the platform build matrix. The job is enabled only when `PORTZILLA_PUBLISH_CARGO` is set to `true`, checks out the workflow ref, verifies that the Cargo package version matches the release tag, and runs `cargo publish --token "$CARGO_REGISTRY_TOKEN"`.

The token is supplied only through the GitHub Actions secret `CARGO_REGISTRY_TOKEN`. No token is stored in the repository or printed by the workflow. The existing npm publication job remains independent and unchanged.

## Verification

Validate the workflow YAML locally, verify the version-check shell script with the current package, and confirm the remote workflow publishes `portzilla@0.2.0` to crates.io.
