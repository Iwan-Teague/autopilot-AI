# Model Tiers — Optional Per-Stage Model Switching

Opt-in feature. When enabled, the skill assigns a model to each stage based
on its complexity, so cheap/fast models handle scaffolding and tests while
expensive/smart models handle real implementation work. The binary respects
`stage.model` in API and CLI modes (forces the model). In webhook mode it
prepends a "Recommended model" hint to the prompt.

## How to enable

When the skill starts, ask the user:

> "Enable per-stage model tiering? Foundation + testing run on a cheap
> model; implementation runs on a smart model. Saves cost on long pipelines.
> [y/N]"

Default off. Only proceed with tiering if the user says yes.

## Tier definitions

| Tier   | When to use it                       | Roughly                       |
|--------|--------------------------------------|-------------------------------|
| light  | Scaffolding, lint, format, tests, docs | Cheapest available — Haiku, gpt-4o-mini, llama-3-8b |
| mid    | Routine implementation, refactors    | Sonnet, gpt-4o, llama-3-70b   |
| heavy  | Complex new code, architecture       | Opus, o1, gpt-5-pro           |

## Default phase → tier mapping

| Phase           | Default tier | Rationale                             |
|-----------------|--------------|---------------------------------------|
| foundation      | light        | Project scaffolding is rote work      |
| implementation  | heavy        | Real coding — pay for quality here    |
| testing         | light        | Run tests, write README, verify build |

Override per-stage based on prompt content:

- Bump up to `heavy` if prompt mentions: `architecture`, `complex`,
  `refactor`, `migrate`, `concurrent`, `lock-free`, `algorithm`,
  `state machine`, `parser`, `compiler`.
- Bump down to `light` if prompt mentions: `lint`, `format`, `style`,
  `comment`, `rename`, `docstring`, `README`, `documentation`,
  `cleanup`, `cosmetic`.

## Workflow when tiering is enabled

1. Run `python3 skill/ai-autopilot/scripts/detect_models.py` — get JSON list of
   `{provider, model, tier, source}` available locally.
2. If the list is empty: tell user, fall back to no tiering.
3. Pick the cheapest model per tier present:
   - `light_model` = first model in detect output with `tier == "light"`
   - `mid_model`   = first with `tier == "mid"` (fallback to `heavy`)
   - `heavy_model` = first with `tier == "heavy"` (fallback to `mid`)
4. For each stage in the pipeline, decide its tier (phase default + keyword
   override) and write `stage.model = <chosen model id>`.
5. Show the assignment in the confirmation summary, e.g.:

   ```
     ── Foundation ──
      1. [foundation]  scaffold project        → claude-haiku-4-5    (light)

     ── Implementation ──
      2. [impl-core]   pure game logic          → claude-opus-4-7    (heavy)
      3. [impl-render] terminal UI + main loop  → claude-sonnet-4-6  (mid)

     ── Testing ──
      4. [test-suite]  cargo test + README      → claude-haiku-4-5    (light)
   ```

   Let the user override before committing pipeline.json.

## Pipeline JSON shape

Add `model` to any stage:

```json
{
  "id": "impl-core",
  "phase": "implementation",
  "summary": "Pure game logic",
  "prompt": "...",
  "model": "claude-opus-4-7"
}
```

The binary's `--model X` flag overrides `pipeline.model` at runtime, but
per-stage `stage.model` always wins.

## Caveats

- **Webhook mode** can't force a model switch. The hint is advisory only —
  the user (or an agent that supports `/model`) has to honor it.
- **API / CLI mode** force the model per stage — full control.
- If the user has only one tier of model available (e.g. only Opus key, no
  Haiku), tiering still works but every stage maps to that one model.
- Model IDs change. Detector probes live endpoints (Ollama, LM Studio) when
  available; for closed APIs it uses a hand-curated default list — keep
  `DEFAULT_BY_KEY` in `detect_models.py` updated.
