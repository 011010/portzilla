# Task 0 findings: OpenCode 2.3.0 plugin hooks

Environment: `opencode` 2.3.0, `opencode run --auto --standalone`, throwaway project with `.opencode/plugin/spy.js`. Spike project lives outside the repo.

## 1. The V1 plugin shape does not load on 2.3.0

Our current shim (`PLUGIN_SNIPPET` in `src/opencode.rs`) exports `async ({ $ }) => ({ "tool.execute.before": ... })`. Loading it produced:

```
failed to load plugin ... PluginModule.LoadError: Plugin must export a default definition with an id and an effect or setup function
```

Consequence: `portzilla init opencode` currently prints a shim that OpenCode 2.3.0 rejects. This is a real bug in Portzilla, independent of file intents.

The same error appears for several plugins in `~/.config/opencode/plugins/` (`engram.ts`, `model-variants.ts`, `skill-registry.ts`, `superpowers.js`). I only read the load log; I did not verify their behavior.

## 2. The V2 shape works

```js
export default {
  id: "spy",
  async setup(ctx) {
    await ctx.tool.hook("execute.before", (event) => { /* event.tool, event.sessionID, event.input */ });
    await ctx.tool.hook("execute.after", (event) => { /* event.status, event.tool */ });
    await ctx.shell.hook("create.before", (event) => { event.env.X = "..." });
  },
};
```

A plain object with `id` and `setup` is enough; importing `@opencode/plugin` was not needed.

## 3. Tool names and argument shapes

| Tool | Name in 2.3.0 | Input fields |
|---|---|---|
| Create file | `write` | `path`, `content` |
| Replace text | `edit` | `path`, `oldString`, `newString` |
| Shell | `shell` (not `bash`) | `command` |
| Read | `read` | `path` |

Observed in the log. A multi-file tool such as `apply_patch` did not appear, so it is still unverified.

Our shim filters on `input.tool === "bash"`, which would never match. It must become `shell`.

## 4. Deny works by throwing

```
execute.before: throw new Error("DENIED BY SPIKE ...")  // when event.tool === "edit" and path matches
```

Result: `forbidden.txt` stayed `original`. The edit was blocked. I did not confirm in the logs what the model received as the tool error.

## 5. `shell.create.before` has no session id

The event keys are `command, cwd, timeout, shell, env`. The V1 `shell.env` hook received `sessionID`; the V2 one does not. Setting `event.env.X` does reach the process: `echo $PZ_SPIKE_SESSION` printed the injected value.

Impact: the `PORTZILLA_SESSION` injection used for own-lease recognition from bash cannot be done the same way. The file-intent design does not need it: the edit hook already receives `sessionID`, so auto-claim works without any shell-side identity.

## Side effect to know about

During the spike the agent also read `spy.log` and called `engram` `mem_save` and `mem_session_summary`, writing two memories titled `pz-spike: ...` into Engram. They are not part of the project and should be deleted.
