# Pipeline JSON Schema

The `pipeline.json` file is the contract between the skill and the Rust binary.

## Top-level

```json
{
  "name": "string — human-readable project name",
  "global_rules": ["array of strings — prepended to every prompt"],
  "interface": "auto | api | cli | accessibility",
  "state_path": "path/to/autopilot-state.json",
  "bootstrap_check": false,
  "stages": [ /* array of Stage objects */ ]
}
```

`bootstrap_check` (optional, default `false`) — when `true`, the kickoff
prompt is a `POST /ready` handshake the AI runs before getting stage 1.
Useful for verifying a new agent works end-to-end. When `false`, the
kickoff prompt IS stage 1 directly — saves a round trip and the tokens
spent on the ceremony.

`branch` (optional) — pin the pipeline to a specific git branch. The
binary refuses to start if the repo's current branch doesn't match.
Override at runtime with `--branch X`. Omit (or null) to operate on
whatever branch is currently checked out. The kickoff summary always
shows the active branch regardless of whether it's pinned.

`auto_paste` (optional, default `false`) — cross-platform. After every
`/stage-complete`, the binary copies the next stage prompt to the
clipboard, activates the target GUI app, and sends paste + send. Each
new stage arrives as a fresh chat turn instead of hidden inside an
HTTP response, so the AI agent has no "summarise mid-pipeline"
offramp. Recommended for any pipeline >8 stages on a chat GUI. Toggle
at runtime with `--auto-paste`.

Platform requirements:
* **macOS** — bundled `pbcopy` + `osascript`. First run requires
  Accessibility permission for the launching terminal.
* **Linux X11** — `xclip` (or `xsel`) + `xdotool`.
* **Linux Wayland** — `wl-copy` + `wtype` (or `ydotool` daemon). User
  must focus the target app within 1.5s of each stage start (Wayland
  blocks programmatic window activation).
* **Windows** — bundled PowerShell + Set-Clipboard + SendKeys. No
  installs.

`target_app` (optional, default `"Claude"`) — display-name substring of
the app `auto_paste` should activate. Common values: `"Claude"`,
`"Codex"`, `"Claude Code"`, `"ChatGPT"`, `"Cursor"`. Match the title
that appears in your OS window manager.

`focus_key` (optional) — keystroke to send right after the target app
activates and before the paste fires. Used to land focus on the chat
input when the previously-focused element was something else (editor
pane, file tree, terminal). Format: `"mod+key"` — e.g. `"cmd+l"`,
`"ctrl+/"`, `"cmd+shift+m"`. Most users don't need this: the binary's
AX walk locates the chat input automatically in most native chat apps.
Set it when the AX walk lands in the wrong place, or when paste sends
into an editor instead of chat.

Modifier names: `cmd` (= `command` / `meta`, mapped to `ctrl` on
Linux/Windows), `ctrl`, `shift`, `alt` (= `option`). Named keys:
`return`, `escape`, `tab`, `space`, `delete`. Otherwise a single
printable character.

## Stage object

```json
{
  "id": "kebab-case-identifier",
  "phase": "foundation | implementation | testing",
  "summary": "One-line description shown in the progress header",
  "prompt": "Full prompt text — global_rules are prepended by the binary",
  "model": "optional — per-stage model override (e.g. claude-haiku-4-5)",
  "timeout_secs": 300
}
```

`timeout_secs` (optional) — overrides the pipeline-level
`stage_timeout_secs` for this stage only. Useful for short stages (lint,
format → 60–300s) or long stages (large refactors → 3600+).

The `model` field is optional. When set:
- **API/CLI mode**: the binary uses this model for the stage, overriding the
  pipeline-level `model` and any auto-detected default.
- **Webhook mode**: the binary prepends a "Recommended model" hint to the
  prompt. The user/agent must honor it via `/model` (advisory only).

See `model-tiers.md` for the opt-in tiering workflow that populates this field.

## Phase values

| Phase            | Description                                    |
|------------------|------------------------------------------------|
| `foundation`     | Scaffolding, deps, shared infrastructure       |
| `implementation` | Feature/section implementation                 |
| `testing`        | Unit, integration, E2E tests and verification  |

## Interface values

| Value             | Behaviour                                            |
|-------------------|------------------------------------------------------|
| `auto`            | API if ANTHROPIC_API_KEY set, else CLI, else AX      |
| `api`             | Anthropic REST API (requires ANTHROPIC_API_KEY)      |
| `cli`             | `claude -p <prompt>` subprocess                      |
| `accessibility`   | macOS Accessibility APIs (browser must be open)      |

## Minimal example

```json
{
  "name": "Todo App",
  "global_rules": [
    "Use Rust for all backend code",
    "Write tests for every public function",
    "Follow the project structure in the spec"
  ],
  "interface": "auto",
  "state_path": "autopilot-state.json",
  "stages": [
    {
      "id": "foundation-setup",
      "phase": "foundation",
      "summary": "Scaffold project, Cargo.toml, DB schema",
      "prompt": "## Context\nThis is the first stage of the Todo App pipeline.\n\n## Objective\nCreate the project scaffold: initialize a Cargo workspace, add sqlx + axum + tokio dependencies, write the database schema in `schema.sql`, and create the top-level module structure.\n\n## Acceptance criteria\n- `Cargo.toml` exists with correct dependencies\n- `schema.sql` defines the todos table\n- `src/main.rs` compiles without errors\n\nWhen you have finished this stage, output exactly:\nSTAGE COMPLETE: foundation-setup"
    },
    {
      "id": "impl-crud",
      "phase": "implementation",
      "summary": "Implement CRUD endpoints for todos",
      "prompt": "## Context\nThe project scaffold from foundation-setup is in place.\n\n## Objective\nImplement the four CRUD endpoints: POST /todos, GET /todos, PUT /todos/:id, DELETE /todos/:id using axum and sqlx.\n\n## Acceptance criteria\n- All four routes exist in `src/routes/todos.rs`\n- Each handler has a corresponding sqlx query\n- Routes are registered in main.rs\n\nWhen you have finished this stage, output exactly:\nSTAGE COMPLETE: impl-crud"
    },
    {
      "id": "test-unit",
      "phase": "testing",
      "summary": "Write and run unit + integration tests",
      "prompt": "## Context\nAll CRUD endpoints are implemented.\n\n## Objective\nWrite unit tests for the handler logic and integration tests using a test database. Run `cargo test` and ensure all tests pass.\n\n## Acceptance criteria\n- `cargo test` exits with code 0\n- At least one test per CRUD operation\n\nWhen you have finished this stage, output exactly:\nSTAGE COMPLETE: test-unit"
    }
  ]
}
```
