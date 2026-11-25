# portzilla — landing page

Static, dependency-free landing page. Three files plus assets; no build step.

```console
$ python3 -m http.server 8731 --directory site
# open http://127.0.0.1:8731/
```

## Structure

| File | Purpose |
|---|---|
| `index.html` | Markup and copy |
| `styles.css` | Design tokens, layout, motion; `prefers-reduced-motion` respected |
| `main.js` | Progressive enhancement only: scroll reveals and the copy button. The page is fully readable with JS disabled |
| `assets/` | Logo and the recorded demo (copies of `../logo.png` and `../docs/assets/portzilla-demo.gif` so the folder can be published as a site root) |

## Publishing

The folder is self-contained, so any static host works. For GitHub Pages, set the
repository's Pages source to the `/site` folder on `main`; the project URL is then
`https://<owner>.github.io/portzilla/`.

If the page moves under a project path, relative asset paths keep working; no
absolute URLs are used except the outbound links to GitHub, crates.io and npm.

## Editing

- Copy lives in `index.html`. Claims about behaviour must stay true to the CLI:
  the three layers, the conflict-aware verdicts, and the "unprovable denies" rule.
- Colors, type and spacing are CSS custom properties at the top of `styles.css`.
- The display face is Fraunces and the mono is Space Mono, both loaded from
  Google Fonts with local fallbacks (`Georgia`, `ui-monospace`).
- The logo ships as opaque RGB on black, so it is presented as a bordered chip in
  the header rather than blended into the background.
