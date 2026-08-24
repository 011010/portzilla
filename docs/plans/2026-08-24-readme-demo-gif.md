# README Demo GIF Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a compact, authentic terminal GIF to the README showing Portzilla resolving a port conflict between two development sessions.

**Architecture:** Generate the animation from a real isolated CLI run, store the final asset under `docs/assets/`, and reference it from `README.md` with a relative Markdown image link. Keep temporary lease state and recording artifacts outside the repository.

**Tech Stack:** Rust CLI, shell recording, `ffmpeg`, animated GIF, GitHub-flavored Markdown.

---

### Task 1: Create the isolated demo recording

**Files:**
- Create: `docs/assets/portzilla-demo.gif`
- Temporary only: `/var/folders/hl/ktc1dc_129537yl571qrgrh80000gn/T/opencode/portzilla-demo/`

**Step 1: Build the current binary**

Run: `cargo build`

Expected: the project builds successfully and produces `target/debug/portzilla`.

**Step 2: Prepare isolated demo state**

Create a temporary data directory outside the repository and ensure the recording uses `PORTZILLA_DATA_DIR` pointing to it. Do not use the user's normal data directory.

**Step 3: Record the real CLI flow**

Record these commands and their output with deliberate pauses:

```console
portzilla claim 3000 --tag next-dev --pid <live-demo-pid>
portzilla claim 3000 --tag vite-dev --pid <second-live-demo-pid>
portzilla who 3001
portzilla ls
```

Expected: the second claim reports that port `3000` is busy and claims the next available port, normally `3001`.

**Step 4: Convert the recording to GIF**

Use `ffmpeg` to crop the terminal to its content, scale it to a readable width, preserve terminal contrast, and limit the palette/frame rate so the final file stays below 2 MB.

Expected: `docs/assets/portzilla-demo.gif` is an animated GIF with an 8–12 second duration.

### Task 2: Embed the demo in the README

**Files:**
- Modify: `README.md` after the introductory value proposition and before `## Quick start`

**Step 1: Add the relative image link**

Add:

```markdown
![Portzilla preventing a port conflict](docs/assets/portzilla-demo.gif)
```

Expected: the README uses a repository-relative path that GitHub can resolve.

**Step 2: Keep the quick-start section intact**

Do not duplicate the full demo output or move the existing install and command documentation. The GIF should provide visual context while the text remains copyable.

### Task 3: Verify the asset and repository state

**Files:**
- Verify: `README.md`
- Verify: `docs/assets/portzilla-demo.gif`

**Step 1: Check the GIF metadata and size**

Run: `file docs/assets/portzilla-demo.gif` and `du -h docs/assets/portzilla-demo.gif`

Expected: the file is recognized as an animated GIF and is smaller than 2 MB.

**Step 2: Inspect the README reference**

Run: `rg -n "portzilla-demo|Quick start" README.md`

Expected: the image reference appears before the Quick start heading and uses `docs/assets/portzilla-demo.gif`.

**Step 3: Verify no temporary state was added**

Run: `git status --short`

Expected: only the intended README, GIF, and plan files are present; no `leases.json`, terminal recording, or temporary files are tracked.

**Step 4: Review the rendered README**

Open the README on GitHub or a compatible Markdown preview and confirm the animation loads, remains readable on desktop/mobile, and communicates the automatic reassignment without requiring the surrounding prose.
