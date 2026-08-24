# README Demo GIF Design

## Goal

Add a short, authentic terminal GIF to the README so visitors can immediately see how `portzilla` prevents two development sessions from taking the same port.

## Approved Approach

Use a real CLI recording rather than a hand-designed animation. The recording will show two claims for port `3000`, the automatic reassignment to `3001`, and inspection with `who` and `ls`.

## Asset

- Path: `docs/assets/portzilla-demo.gif`
- Format: animated GIF for automatic playback in GitHub Markdown
- Target duration: 8–12 seconds
- Target size: less than 2 MB
- Content: terminal commands and real `portzilla` output only
- Accessibility: README image includes descriptive alt text

## README Placement

Place the GIF after the introductory value proposition and before `Quick start`, using a relative path:

```markdown
![Portzilla preventing a port conflict](docs/assets/portzilla-demo.gif)
```

## Recording Flow

```text
portzilla claim 3000 --tag next-dev
portzilla claim 3000 --tag vite-dev
portzilla who 3001
portzilla ls
```

The demo should use an isolated `PORTZILLA_DATA_DIR` and explicit PIDs or live processes so it does not modify the user's real lease store and does not depend on unrelated processes.

## Verification

- The GIF exists at the documented path.
- The README references the GIF with a relative path.
- The second claim visibly receives the next available port.
- The animation opens successfully and remains within the target size.
- The source tree does not include temporary lease state or recording files.
