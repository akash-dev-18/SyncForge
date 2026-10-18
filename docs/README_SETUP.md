# SyncEngine OpenCode Multi-Agent Starter Kit

This bundle provides project-local OpenCode agents, task-routing rules, model assignments, and per-agent automatic fallback chains for SyncEngine.

## Contents

- `opencode.json` — project default model, fallback plugin, subagent depth, and per-agent fallback chains.
- `.opencode/agents/*.md` — 13 specialist subagents.
- `.opencode/opencode-fallback.jsonc` — global fallback defaults.
- `AGENTS.md` — task routing and SyncEngine safety/learning instructions.
- `docs/MODEL_ROUTING.md` — model assignment assumptions and privacy notes.

## Install into your SyncEngine repository

1. Back up any existing `opencode.json`, `AGENTS.md`, and `.opencode/` content. Merge instead of blindly overwriting your current configuration.
2. Copy the contents of this bundle into the root of the SyncEngine repository.
3. Ensure OpenCode Zen is connected and the eight IDs shown in `docs/MODEL_ROUTING.md` appear in `/models`. The IDs are for the OpenCode provider (`opencode/<model-id>`), not arbitrary aliases.
4. The config includes `opencode-runtime-fallback` in the plugin list. OpenCode normally installs npm plugins configured this way at startup; the upstream plugin documents this package name. If it fails to load, check the plugin logs and the plugin's current upstream setup instructions before changing other settings. Do not install two different model-fallback plugins at the same time.

5. Restart OpenCode from the project root.
6. Verify the model IDs using `/models` or `opencode models`. Try `@architect` and `@rust-mentor` on a harmless read-only task.
7. Test fallback safely in a disposable test session by temporarily configuring a test agent with an unavailable model, verify the fallback notification/log, and then restore the real model. Never test failure recovery in the middle of a destructive editing task.

## Notes

- `subagent_depth: 2` allows a subagent to invoke one further level of subagents. Use nested delegation sparingly; excessive nesting adds latency and creates noisy context.
- Per-agent primary models are set in Markdown frontmatter. Their fallback chains are keyed by the exact same agent names in `opencode.json`.
- The main `build` agent inherits the root model. It is explicitly given permission to invoke subagents through task routing.
- `opencode-runtime-fallback` is a third-party community plugin, not a built-in OpenCode feature. Its behavior depends on the installed OpenCode/plugin versions. Review its source, releases and permissions before relying on it.
- A model fallback can restart a failed turn and does not guarantee recovery of an incomplete tool call. Review diffs and run tests.
- Free models' availability and privacy terms may change. Never send secrets or sensitive customer/user data to them.
