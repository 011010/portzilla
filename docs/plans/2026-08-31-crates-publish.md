# crates.io Publish Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Publish `portzilla` to crates.io from the existing release workflow when explicitly enabled.

**Architecture:** Add a Cargo publication job after the release build matrix. The job is gated by a repository variable, checks the package version against `RELEASE_TAG`, and reads the crates.io token only from the `CARGO_REGISTRY_TOKEN` GitHub secret. npm publication remains a separate job.

**Tech Stack:** GitHub Actions, Cargo, crates.io registry.

---

### Task 1: Add Cargo publication job

**Files:**
- Modify: `.github/workflows/release.yml:134-159`

**Step 1: Add the gated job**

Add `publish-cargo` after the build job with `needs: build`, `runs-on: ubuntu-latest`, and `if: ${{ vars.PORTZILLA_PUBLISH_CARGO == 'true' }}`.

**Step 2: Check out the release source**

Use `actions/checkout@v4` with `ref: ${{ env.SOURCE_REF }}` and install the stable Rust toolchain.

**Step 3: Verify the tag version**

Extract the manifest version with `cargo pkgid -p portzilla | sed 's/.*[#@]//'`, strip the leading `v` from `RELEASE_TAG`, and fail if they differ.

**Step 4: Publish using the secret**

Run `cargo publish --token "$CARGO_REGISTRY_TOKEN"` with `CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}` in the step environment.

### Task 2: Verify locally

**Files:**
- Verify: `.github/workflows/release.yml`

**Step 1: Check formatting and YAML**

Run `git diff --check` and parse the workflow with the repository's available YAML tooling.

**Step 2: Verify the version command**

Run the Cargo version extraction against the current checkout and confirm it returns `0.2.0`.

### Task 3: Publish and verify remotely

**Files:**
- No source files.

**Step 1: Commit and push the workflow change**

Use a conventional commit and push `feat/lease-identity`.

**Step 2: Enable Cargo publication**

Set repository variable `PORTZILLA_PUBLISH_CARGO=true` and secret `CARGO_REGISTRY_TOKEN` outside the repository.

**Step 3: Rerun the release workflow**

Dispatch `release.yml` for `v0.2.0` and wait for all jobs.

**Step 4: Confirm publication**

Verify the workflow is successful and `cargo search portzilla --limit 1` reports `0.2.0`.
