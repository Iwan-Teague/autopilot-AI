---
name: ai-autopilot
description: >
  Automated AI prompt pipeline builder for locally-running AI agents. Use this
  skill when the user wants to run a long, multi-stage project through an AI
  agent autonomously — without needing to be present for each prompt. The skill
  reads a project specification document (.md), extracts rules and pipeline
  stages (foundation → implementation → testing), generates all prompts, shows
  the user a summary for confirmation, then hands off to the autopilot Rust
  binary which manages the pipeline from there.

  Works with any locally-running AI that has shell access: Claude Code, Codex,
  Ollama, LM Studio, or any agent that can run a curl command.

  Trigger whenever you hear: "run this automatically", "prompt pipeline",
  "autopilot", "hands-free project build", "keep prompting until done",
  "auto-run my spec", or any request to execute a multi-hour project doc
  without the user staying at the keyboard.
---

# ai-autopilot Skill

Turns a project specification document into an ordered prompt pipeline and
drives a local AI agent through it end-to-end — no human needed after the
first prompt is pasted.

## How it works

```
1. User provides a project spec (.md)
2. You parse it → extract rules + pipeline stages
3. You generate a prompt for each stage
4. You show the user a summary and ask for confirmation
5. You write pipeline.json and build the Rust binary
6. User pastes the first prompt into their AI agent
7. The agent self-chains through every stage via the local HTTP server
```

## Interfaces

Three modes — all target locally-running AI, no browser automation:

| Mode | How it works | Best for |
|---|---|---|
| **webhook** (default) | Binary serves prompts via HTTP. AI runs `curl /stage-complete` when done, gets next prompt in response. AI self-chains. | Claude Code, Codex, any agent with shell access |
| **api** | Binary calls the provider REST API directly, manages the full conversation. | Headless / scripted use |
| **cli** | Binary spawns `claude -p` or `codex -p` as a subprocess per stage. | Simple single-turn agents |

Auto-detection tries webhook → API → CLI in that order.

---

## Step 1 — Read and understand the spec document

Ask for the project spec file if not provided. Once you have it:

- Read the full document.
- Identify the **Project Rules** section — constraints, style guides, and
  requirements that must apply to every stage. If there is no explicit rules
  section, infer 3–6 rules from the document's goals and constraints.
- Identify **all project sections** — features, modules, or components to build.

### Always-on output rules

Append these two rules to `global_rules` on every pipeline (cuts ~30%
of output tokens by suppressing model preamble/recap):

1. *"Be terse. Output code, diffs, file edits, or shell commands only — no
   preamble, no recap, no narration. The required `curl /stage-complete`
   call at the end of each stage (with a one-sentence factual summary) is
   the only exception — always include it."*
2. *"For modifications under ~50 lines, output a unified diff or use targeted
   edits — do not paste full file contents. Full files are fine for new
   files or large rewrites."*

These ship in the cacheable `system` block, so on Anthropic API mode they
cost almost nothing after stage 1.

---

## Step 2 — Generate the prompt pipeline

Organise stages into exactly three phases in order:

### Phase 1: Foundation
One stage (occasionally two) covering:
- Project scaffolding and directory structure
- Core dependencies and configuration
- Shared infrastructure (data models, DB schema, API clients, auth, etc.)

The foundation must be complete enough that implementation stages can build on
it without revisiting setup decisions.

### Phase 2: Implementation
One stage per major section/feature from the spec. Each prompt should:
- State clearly what it is building (name the spec section)
- Assume the foundation stage is already in place
- Have an unambiguous definition of done

Split large sections into multiple stages rather than making one stage too broad.

### Phase 3: Testing
One or more stages covering:
- Unit tests for core logic
- Integration tests for cross-component behaviour
- End-to-end tests or smoke-test checklist
- Build/CI verification

The final testing stage should verify the project is in a working, usable state.

---

## Step 3 — Compose each prompt

Every prompt must include:

1. `## Context` — which phase/stage this is and what preceded it
2. `## Objective` — exactly what to build
3. `## Acceptance criteria` — binary pass/fail checks (file exists, tests pass, etc.)
4. A reference to the relevant spec section(s) by name

Global rules are prepended automatically by the binary — do not repeat them
in individual stage prompts.

### Small-edit stages — request diff output explicitly

If the stage prompt describes a *modification* rather than greenfield code
(keywords: `fix`, `tweak`, `adjust`, `rename`, `update`, `add to`, `replace`,
`refactor`, `inline`, `extract`), append this line to the `## Objective`:

> *Output a unified diff or targeted file edits only — do not paste full
> file contents.*

Skip this for stages that create new files or large modules — full-file
output is fine there.

### Completion marker

Every prompt must end with this block (substituting the real stage ID and port):

```
When you have fully completed the objectives above, run this curl. Do NOT
narrate the result back to the user — the response IS your next task.

```bash
curl -s -X POST http://localhost:7432/stage-complete \
  -H "Content-Type: application/json" \
  -d '{"stage_id":"STAGE_ID","summary":"One sentence: what you built."}' | cat
```

Then act on the response in this same turn, without pausing:
- `"status": "continue"` → the field `next_stage.prompt` is your IMMEDIATE
  next task. Start executing it now. Do not ask for user confirmation.
  Do not paste it back to the user. Just begin.
- `"status": "complete"` → the pipeline is finished, report success briefly.
- `"status": "error"` → read `message`, fix the issue, retry the curl.
```

The `summary` field is a single sentence describing what was built or tested —
it's written to `autopilot-summary.md` as each stage completes. Keep it factual
and brief (the full response is in the chat history).

In webhook mode the binary builds the kickoff prompt automatically:

* **Default (auto-start):** the kickoff prompt IS stage 1, with a one-paragraph
  preamble explaining the self-chain protocol. The AI reads it and starts
  executing — no `/ready` handshake, one fewer round trip, fewer tokens.
* **Opt-in `bootstrap_check: true`** (or `--bootstrap-check` CLI flag): the
  kickoff is a `POST /ready` handshake. The AI verifies the connection first,
  receives stage 1 in the JSON response, then begins. Useful the first time
  you wire up a new agent and want to verify the protocol works.

Stages 2+ receive their next prompt in the `/stage-complete` response body.

---

## Step 3.5 — Per-stage model tiering (optional, opt-in)

**You must ask this question before showing the Step 4 summary.** Don't
assume the answer either way — wait for an explicit reply.

> "Enable per-stage model tiering? Cheap models run scaffolding + tests;
> smart models run implementation. Saves cost on long pipelines. [y/N]"

If the user says no, skip the rest of this step — every stage runs on
whatever single model is configured. If the user says yes, proceed below.

If the user says yes:

1. Run the detector:
   ```bash
   python3 /path/to/skill/ai-autopilot/scripts/detect_models.py
   ```
   It emits JSON: `[{provider, model, tier, source}, ...]`.
2. If the output is empty, tell the user no models were detected and fall
   back to no tiering (don't fail the pipeline).
3. Otherwise pick one model per tier:
   - `light` → first entry with `tier == "light"`
   - `mid`   → first with `tier == "mid"` (fallback to `heavy`)
   - `heavy` → first with `tier == "heavy"` (fallback to `mid`)
4. Assign each stage a tier:
   - Foundation → `light` by default
   - Implementation → `heavy` by default
   - Testing → `light` by default
   - Bump **up** if prompt contains: `architecture`, `complex`, `refactor`,
     `migrate`, `algorithm`, `state machine`, `parser`, `compiler`.
   - Bump **down** if prompt contains: `lint`, `format`, `rename`,
     `docstring`, `README`, `cleanup`.
5. Write `stage.model = <model id for that tier>` into each stage in
   pipeline.json.

Show the user the model assignment in the Step 4 confirmation summary so
they can override before kickoff.

See `references/model-tiers.md` for the full tier table and rationale.

---

## Step 4 — Show summary and get confirmation

Present the full pipeline before writing any files:

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  PROJECT AUTOPILOT — <project name>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Global rules (<N> rules prepended to every prompt):
    1. <rule>
    ...

  ── Foundation ──
   1. [foundation-setup]  <summary>

  ── Implementation ──
   2. [impl-<name>]       <summary>
   ...

  ── Testing ──
   N. [test-suite]        <summary>
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Stages: <N>  |  Interface: webhook (auto)
  Model tiering: <enabled — show per-stage model> | disabled
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Does this look right? Any stages to add, remove, or rename?
Once confirmed I'll generate pipeline.json and kick it off.
```

Wait for explicit confirmation before proceeding.

---

## Step 5 — Generate pipeline.json

Run the parser script:

```bash
python3 /path/to/skill/ai-autopilot/scripts/parse_pipeline.py \
  --spec <path-to-spec.md> \
  --output pipeline.json
```

Or write pipeline.json directly. Structure:

```json
{
  "name": "My Project",
  "global_rules": ["Rule 1", "Rule 2"],
  "interface": "webhook",
  "provider": "auto",
  "state_path": "autopilot-state.json",
  "stages": [
    {
      "id": "foundation-setup",
      "phase": "foundation",
      "summary": "Scaffold project, install deps, create core structure",
      "prompt": "## Context\n...\n## Objective\n...\n## Acceptance criteria\n...\nWhen you have finished this stage, output exactly:\nSTAGE COMPLETE: foundation-setup"
    }
  ]
}
```

See `references/pipeline-schema.md` for the full schema.

---

## Step 6 — Build and run

Build once (or after source changes):

```bash
cd /Users/iwan/Desktop/autopilot-ai && cargo build --release 2>&1
```

**Webhook mode — recommended for Claude Code / Codex / any local agent:**

```bash
./target/release/autopilot --pipeline pipeline.json
```

The binary prints the first prompt. Paste it into your AI agent. It will work
through every stage automatically, calling the local server after each one.

Check progress at any time:
```bash
curl -s http://localhost:7432/status | python3 -m json.tool
```

Resume an interrupted run:
```bash
autopilot --pipeline pipeline.json --resume
```

Force a specific interface or provider:
```bash
autopilot --pipeline pipeline.json --interface api
autopilot --pipeline pipeline.json --interface cli
autopilot --pipeline pipeline.json --port 8080
```

---

## Provider auto-detection

The binary detects which AI provider to use automatically — no configuration
needed for common setups. Detection order:

1. **API keys in environment** (first match wins):

| Env var | Provider | Default model |
|---|---|---|
| `ANTHROPIC_API_KEY` | Anthropic | `claude-opus-4-6` |
| `OPENAI_API_KEY` | OpenAI | `gpt-4o` |
| `MISTRAL_API_KEY` | Mistral | `mistral-large-latest` |
| `GROQ_API_KEY` | Groq | `llama-3.3-70b-versatile` |
| `TOGETHER_API_KEY` | Together AI | `meta-llama/Llama-3-70b-chat-hf` |

2. **Local services** — Ollama (port 11434), LM Studio (port 1234)
3. **Local CLIs** — `claude`, `codex`

Override in pipeline.json when needed:
```json
{ "provider": "ollama", "model": "llama3.2" }
{ "provider": "custom", "api_base_url": "http://my-server:8080", "model": "my-model" }
```

Or via environment:
- `AUTOPILOT_MODEL` — override model for any provider
- `OLLAMA_HOST` — Ollama base URL (default: `http://localhost:11434`)
- `RUST_LOG=debug` — verbose logging

---

## Writing good stage prompts

- **Be specific about file paths** — "create `src/auth/mod.rs`" not "the auth module"
- **Reference the spec** — "as described in the Authentication section"
- **State what already exists** — "the DB schema from Foundation is in `schema.sql`"
- **Acceptance criteria must be binary** — "all tests pass" or "file X exists", not "looks right"
- **Keep stages independent** — each should work given only the foundation

---

## References

- `references/pipeline-schema.md` — full JSON schema with examples
- `references/example-spec.md` — example project spec to try
- `references/model-tiers.md` — opt-in per-stage model switching
- `scripts/detect_models.py` — probe local env for available AI models
- Rust source: `/Users/iwan/Desktop/autopilot-ai/src/`
