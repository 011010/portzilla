# Multiplatform CI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Run Portzilla's test suite natively on Linux, macOS, and Windows while keeping quality checks and dependency auditing in a single Linux job.

**Architecture:** Preserve the existing `verify` job for formatting, Clippy, tests, and RustSec auditing. Add a separate `test` job with an Ubuntu/macOS/Windows matrix that runs all Cargo targets and features on each host OS. Keep release packaging unchanged.

**Tech Stack:** GitHub Actions, stable Rust, Cargo, `dtolnay/rust-toolchain`, `Swatinem/rust-cache`.

---

### Task 1: Add the native operating-system test matrix

**Files:**
- Modify: `.github/workflows/ci.yml`

**Step 1: Define the matrix job**

Add a `test` job using `strategy.matrix.os` with `ubuntu-latest`, `macos-latest`, and `windows-latest`. Give the job a matrix-specific display name so failures identify the host operating system.

**Step 2: Install Rust and cache per matrix entry**

Use the existing stable Rust toolchain action and Rust cache action in the matrix job. Do not add target-specific cross-compilation because each job runs natively.

**Step 3: Run the complete native test suite**

Run:

```yaml
cargo test --all-targets --all-features
```

Keep the existing Linux `verify` test command unchanged until the matrix is proven, so the quality job remains an independent regression signal.

**Step 4: Inspect the workflow diff**

Run:

```bash
git diff -- .github/workflows/ci.yml
```

Expected: only the new native test matrix is added; release workflow and existing quality checks are unchanged.

### Task 2: Validate the workflow and local checks

**Files:**
- Test: `.github/workflows/ci.yml`

**Step 1: Check YAML syntax and action structure**

Use an available YAML parser or GitHub Actions workflow linter. Confirm that the matrix values are valid runner labels and that every matrix job has checkout, Rust setup, cache, and test steps.

**Step 2: Run local quality checks**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
git diff --check
```

Expected: all commands exit successfully. The local macOS run cannot substitute for native Windows CI, but it verifies that the workflow change does not alter the test suite.

**Step 3: Review changed files**

Run:

```bash
git status --short
```

Expected: `.github/workflows/ci.yml`, the approved design document, and this implementation plan are the only intended changes beyond pre-existing P0 modifications.

**Step 4: Commit when explicitly authorized**

Do not commit while the current no-commit restriction remains active. When authorized, commit the CI work as a focused conventional commit:

```bash
```
