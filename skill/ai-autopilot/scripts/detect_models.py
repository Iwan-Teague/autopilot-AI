#!/usr/bin/env python3
"""
detect_models.py — Probe the local environment for available AI models.

Output: JSON list of {provider, model, tier, source} sorted by tier
(light → mid → heavy) so the skill can pick a default per stage type.

Probes (each is best-effort, failures are silent):
  • Env vars: ANTHROPIC_API_KEY, OPENAI_API_KEY, MISTRAL_API_KEY,
    GROQ_API_KEY, TOGETHER_API_KEY → enables that provider's tier table.
  • Ollama:   GET http://localhost:11434/api/tags → installed models.
  • LM Studio: GET http://localhost:1234/v1/models → loaded models.
  • CLIs:     `which claude`, `which codex` → enables CLI subprocess mode.

Usage:
  python3 detect_models.py            # JSON to stdout
  python3 detect_models.py --pretty   # human-readable
"""

from __future__ import annotations
import json
import os
import shutil
import subprocess
import sys
import urllib.request
import urllib.error

TIERS = ["light", "mid", "heavy"]
TIER_ORDER = {t: i for i, t in enumerate(TIERS)}

# Hand-curated tier table. Match by `model.startswith(prefix)`.
# Keep this short — only the models people actually use.
KNOWN_MODELS: list[tuple[str, str, str]] = [
    # (prefix, provider, tier)
    # Anthropic
    ("claude-haiku",          "anthropic", "light"),
    ("claude-3-haiku",        "anthropic", "light"),
    ("claude-3-5-haiku",      "anthropic", "light"),
    ("claude-sonnet",         "anthropic", "mid"),
    ("claude-3-5-sonnet",     "anthropic", "mid"),
    ("claude-3-7-sonnet",     "anthropic", "mid"),
    ("claude-opus",           "anthropic", "heavy"),
    ("claude-3-opus",         "anthropic", "heavy"),
    # OpenAI
    ("gpt-4o-mini",           "openai", "light"),
    ("gpt-5-mini",            "openai", "light"),
    ("gpt-4-mini",            "openai", "light"),
    ("o1-mini",               "openai", "light"),
    ("gpt-4o",                "openai", "mid"),
    ("gpt-5",                 "openai", "mid"),
    ("o3-mini",               "openai", "mid"),
    ("o1",                    "openai", "heavy"),
    ("o3",                    "openai", "heavy"),
    ("gpt-5-pro",             "openai", "heavy"),
    # Mistral
    ("mistral-small",         "mistral", "light"),
    ("mistral-medium",        "mistral", "mid"),
    ("mistral-large",         "mistral", "heavy"),
    # Groq / Together (Llama family)
    ("llama-3.2-1b",          "local",   "light"),
    ("llama-3.2-3b",          "local",   "light"),
    ("llama-3.1-8b",          "local",   "light"),
    ("llama-3-8b",            "local",   "light"),
    ("llama-3.3-70b",         "local",   "mid"),
    ("llama-3.1-70b",         "local",   "mid"),
    ("llama-3-70b",           "local",   "mid"),
    ("llama-3.1-405b",        "local",   "heavy"),
]

# Default models to surface when only an API key is present (no live list call).
DEFAULT_BY_KEY: dict[str, list[tuple[str, str]]] = {
    # env_var: [(model, tier), ...]
    "ANTHROPIC_API_KEY": [
        ("claude-haiku-4-5",  "light"),
        ("claude-sonnet-4-6", "mid"),
        ("claude-opus-4-7",   "heavy"),
    ],
    "OPENAI_API_KEY": [
        ("gpt-4o-mini", "light"),
        ("gpt-4o",      "mid"),
        ("o1",          "heavy"),
    ],
    "MISTRAL_API_KEY": [
        ("mistral-small-latest",  "light"),
        ("mistral-medium-latest", "mid"),
        ("mistral-large-latest",  "heavy"),
    ],
    "GROQ_API_KEY": [
        ("llama-3.1-8b-instant",   "light"),
        ("llama-3.3-70b-versatile","mid"),
    ],
    "TOGETHER_API_KEY": [
        ("meta-llama/Llama-3-8b-chat-hf",  "light"),
        ("meta-llama/Llama-3-70b-chat-hf", "mid"),
    ],
}


def tier_for(model: str) -> str:
    """Best-effort tier guess from a model id. Defaults to 'mid' on no match."""
    m = model.lower()
    for prefix, _provider, tier in KNOWN_MODELS:
        if m.startswith(prefix):
            return tier
    return "mid"


def probe_env_keys() -> list[dict]:
    out: list[dict] = []
    for env, models in DEFAULT_BY_KEY.items():
        if os.environ.get(env):
            provider = env.replace("_API_KEY", "").lower()
            for model, tier in models:
                out.append({
                    "provider": provider,
                    "model": model,
                    "tier": tier,
                    "source": f"env:{env}",
                })
    return out


def probe_ollama() -> list[dict]:
    host = os.environ.get("OLLAMA_HOST", "http://localhost:11434").rstrip("/")
    try:
        with urllib.request.urlopen(f"{host}/api/tags", timeout=1.5) as resp:
            data = json.loads(resp.read())
    except (urllib.error.URLError, OSError, json.JSONDecodeError):
        return []
    out = []
    for m in data.get("models", []):
        name = m.get("name") or m.get("model")
        if not name:
            continue
        out.append({
            "provider": "ollama",
            "model": name,
            "tier": tier_for(name),
            "source": f"ollama:{host}",
        })
    return out


def probe_lm_studio() -> list[dict]:
    try:
        with urllib.request.urlopen("http://localhost:1234/v1/models", timeout=1.5) as resp:
            data = json.loads(resp.read())
    except (urllib.error.URLError, OSError, json.JSONDecodeError):
        return []
    out = []
    for m in data.get("data", []):
        mid = m.get("id")
        if not mid:
            continue
        out.append({
            "provider": "lm_studio",
            "model": mid,
            "tier": tier_for(mid),
            "source": "lm_studio:1234",
        })
    return out


def probe_clis() -> list[dict]:
    out = []
    if shutil.which("claude"):
        out.append({
            "provider": "claude_cli",
            "model": "claude-opus-4-7",
            "tier": "heavy",
            "source": "cli:claude",
        })
        out.append({
            "provider": "claude_cli",
            "model": "claude-sonnet-4-6",
            "tier": "mid",
            "source": "cli:claude",
        })
        out.append({
            "provider": "claude_cli",
            "model": "claude-haiku-4-5",
            "tier": "light",
            "source": "cli:claude",
        })
    if shutil.which("codex"):
        out.append({
            "provider": "codex_cli",
            "model": "o4-mini",
            "tier": "light",
            "source": "cli:codex",
        })
    return out


def detect() -> list[dict]:
    seen: set[tuple[str, str]] = set()
    result: list[dict] = []
    for entry in probe_env_keys() + probe_ollama() + probe_lm_studio() + probe_clis():
        key = (entry["provider"], entry["model"])
        if key in seen:
            continue
        seen.add(key)
        result.append(entry)
    result.sort(key=lambda e: (TIER_ORDER.get(e["tier"], 99), e["provider"], e["model"]))
    return result


def main() -> int:
    pretty = "--pretty" in sys.argv
    models = detect()
    if pretty:
        if not models:
            print("No AI models detected.", file=sys.stderr)
            print("Set an API key, run Ollama/LM Studio, or install `claude`/`codex`.", file=sys.stderr)
            return 1
        by_tier: dict[str, list[dict]] = {t: [] for t in TIERS}
        for m in models:
            by_tier[m["tier"]].append(m)
        for tier in TIERS:
            entries = by_tier[tier]
            if not entries:
                continue
            print(f"\n{tier.upper()}  ({len(entries)} model{'s' if len(entries) != 1 else ''})")
            for e in entries:
                print(f"  {e['provider']:12} {e['model']:40}  ({e['source']})")
        print()
        return 0
    json.dump(models, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0 if models else 1


if __name__ == "__main__":
    sys.exit(main())
