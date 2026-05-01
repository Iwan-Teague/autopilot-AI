#!/usr/bin/env python3
"""
parse_pipeline.py — Convert a project spec Markdown file into a pipeline.json
suitable for the autopilot binary.

The AI (Claude following the ai-autopilot skill) typically generates the
pipeline.json directly. This script is provided as a convenience for
re-generating or debugging the config from the raw spec.

Usage:
    python3 parse_pipeline.py --spec project-spec.md --output pipeline.json

The script does a best-effort parse. Claude following the skill should review
and refine the output before passing it to the binary.
"""

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--spec", required=True, type=Path, help="Path to the project spec .md file")
    p.add_argument("--output", required=True, type=Path, help="Where to write pipeline.json")
    p.add_argument(
        "--interface",
        default="webhook",
        choices=["auto", "webhook", "api", "cli", "accessibility"],
        help="AI interface (default: webhook — recommended for desktop AI agents)",
    )
    p.add_argument(
        "--provider",
        default="auto",
        choices=["auto", "anthropic", "open_ai", "ollama", "mistral", "groq", "together", "lm_studio", "custom"],
        help="AI provider for API mode (default: auto-detect from environment)",
    )
    p.add_argument("--model", default=None, help="Model name override (e.g. llama3.2, gpt-4o)")
    p.add_argument("--api-base-url", default=None, help="Custom API base URL (for provider=custom)")
    p.add_argument("--api-key-env", default=None, help="Env var name holding the API key")
    p.add_argument(
        "--state-path",
        default="autopilot-state.json",
        help="Where the binary saves runtime state (default: autopilot-state.json)",
    )
    return p.parse_args()


# ---------------------------------------------------------------------------
# Spec parsing
# ---------------------------------------------------------------------------

RULES_HEADERS = re.compile(
    r"^#{1,3}\s*(project\s+rules?|rules?|constraints?|requirements?|guidelines?)\s*$",
    re.IGNORECASE,
)
SECTIONS_HEADERS = re.compile(
    r"^#{1,3}\s*(sections?|features?|modules?|components?|workstreams?|tasks?)\s*$",
    re.IGNORECASE,
)
TESTING_HEADERS = re.compile(
    r"^#{1,3}\s*(test(ing)?(\s+requirements?)?|qa|verification)\s*$",
    re.IGNORECASE,
)
OVERVIEW_HEADERS = re.compile(
    r"^#{1,3}\s*(overview|introduction|about|summary|description)\s*$",
    re.IGNORECASE,
)
NUMBERED_SECTION = re.compile(r"^#{2,4}\s*\d+\.\s+(.+)$")
BULLET_RULE = re.compile(r"^[-*]\s+(.+)$")


def extract_project_name(lines: list[str]) -> str:
    """First H1 heading, or the filename stem."""
    for line in lines:
        if line.startswith("# "):
            return line[2:].strip()
    return "Untitled Project"


def extract_block(lines: list[str], header_pattern: re.Pattern) -> list[str]:
    """Return all lines between a matching header and the next same-or-higher heading."""
    in_block = False
    block_depth = 0
    result = []

    for line in lines:
        heading_match = re.match(r"^(#{1,6})\s", line)

        if heading_match and header_pattern.match(line.strip()):
            in_block = True
            block_depth = len(heading_match.group(1))
            continue

        if in_block:
            if heading_match:
                depth = len(heading_match.group(1))
                if depth <= block_depth:
                    break  # next sibling or parent heading — stop
            result.append(line)

    return result


def parse_rules(lines: list[str]) -> list[str]:
    """Extract rules from the rules block as a list of strings."""
    block = extract_block(lines, RULES_HEADERS)
    rules = []
    for line in block:
        m = BULLET_RULE.match(line.strip())
        if m:
            rules.append(m.group(1).strip())
        elif re.match(r"^\d+\.\s+", line.strip()):
            rules.append(re.sub(r"^\d+\.\s+", "", line.strip()))
    return rules


def parse_sections(lines: list[str]) -> list[dict]:
    """
    Extract implementation sections. Returns list of:
        {"name": str, "content": str}
    """
    block = extract_block(lines, SECTIONS_HEADERS)
    sections = []
    current_name: Optional[str] = None
    current_lines: list[str] = []

    def flush():
        if current_name:
            sections.append({
                "name": current_name,
                "content": "\n".join(current_lines).strip(),
            })

    for line in block:
        m = NUMBERED_SECTION.match(line.strip())
        if m:
            flush()
            current_name = m.group(1).strip()
            current_lines = []
        elif re.match(r"^#{2,4}\s", line):
            flush()
            current_name = re.sub(r"^#{2,4}\s+", "", line).strip()
            current_lines = []
        else:
            if current_name is not None:
                current_lines.append(line)
    flush()
    return sections


def parse_testing_requirements(lines: list[str]) -> str:
    """Return the testing block as raw text."""
    block = extract_block(lines, TESTING_HEADERS)
    return "\n".join(block).strip()


def parse_overview(lines: list[str]) -> str:
    """Return the overview block as raw text (used in foundation prompt context)."""
    block = extract_block(lines, OVERVIEW_HEADERS)
    return "\n".join(block).strip()


# ---------------------------------------------------------------------------
# Prompt generation
# ---------------------------------------------------------------------------

COMPLETION_MARKER = (
    "When you have fully completed the objectives above, run:\n"
    "```bash\n"
    "curl -s -X POST http://localhost:7432/stage-complete \\\n"
    "  -H \"Content-Type: application/json\" \\\n"
    "  -d '{{\"stage_id\":\"{stage_id}\",\"summary\":\"One sentence describing what you built.\"}}' | cat\n"
    "```\n"
    "Replace the summary with a single factual sentence about what was accomplished.\n"
    "Read the response: if `status` is `continue` start `next_stage.prompt` immediately; "
    "if `complete` you are done; if `error` fix the issue and retry."
)


def make_foundation_prompt(project_name: str, overview: str, rules: list) -> str:
    overview_text = overview if overview else "(see project spec)"
    marker = COMPLETION_MARKER.format(stage_id="foundation-setup")
    return (
        "## Context\n"
        "This is the **Foundation** stage of the " + project_name + " pipeline.\n"
        "Your job is to set up everything that subsequent implementation stages will build on.\n\n"
        "## Project overview\n"
        + overview_text + "\n\n"
        "## Objective\n"
        "1. Create the project directory structure and any necessary config files.\n"
        "2. Set up all dependencies (package manager files, lock files, etc.).\n"
        "3. Implement any shared infrastructure that all other stages will rely on:\n"
        "   - Core data models / types\n"
        "   - Database schema (if applicable)\n"
        "   - Shared utilities, error types, config loading\n"
        "   - API clients or service connections\n"
        "4. Make sure the project builds/compiles without errors at the end of this stage.\n\n"
        "## Acceptance criteria\n"
        "- The project compiles / installs dependencies without errors.\n"
        "- The directory structure matches the spec.\n"
        "- All shared infrastructure is in place for subsequent stages.\n\n"
        + marker
    )


def make_implementation_prompt(project_name: str, section: dict, index: int, total_sections: int) -> str:
    stage_id = kebab(section["name"])
    section_content = section["content"] if section["content"] else "(see project spec for details)"
    marker = COMPLETION_MARKER.format(stage_id="impl-" + stage_id)
    return (
        "## Context\n"
        "This is implementation stage " + str(index) + "/" + str(total_sections)
        + " for the **" + project_name + "** project.\n"
        "The Foundation stage is complete — the project compiles and shared infrastructure is in place.\n\n"
        "## Objective — " + section["name"] + "\n"
        + section_content + "\n\n"
        "## Instructions\n"
        "- Build only what is described above for this section.\n"
        "- Reuse types, utilities, and infrastructure from the Foundation stage.\n"
        "- Follow the project rules that were provided at the start of this pipeline.\n"
        "- Do not modify Foundation-stage files unless strictly necessary; add new files instead.\n\n"
        "## Acceptance criteria\n"
        "- The feature described in this section is fully implemented.\n"
        "- The project still compiles / runs after your changes.\n"
        "- Any new functions have doc comments.\n\n"
        + marker
    )


def make_testing_prompt(project_name: str, testing_content: str) -> str:
    default_tests = (
        "- Unit tests for core logic\n"
        "- Integration tests for cross-component behaviour\n"
        "- End-to-end tests or smoke-test checklist"
    )
    test_text = testing_content if testing_content else default_tests
    marker = COMPLETION_MARKER.format(stage_id="test-suite")
    return (
        "## Context\n"
        "All implementation stages for **" + project_name + "** are complete.\n"
        "This is the final **Testing** stage.\n\n"
        "## Objective\n"
        "Write and run a comprehensive test suite covering the full project:\n\n"
        + test_text + "\n\n"
        "## Instructions\n"
        "- Write tests in the same language/framework as the project.\n"
        "- Use an in-memory or temporary database/state for isolation where applicable.\n"
        "- Run the full test suite and fix any failures before marking this stage complete.\n"
        "- Output a brief summary of test results (pass/fail counts).\n\n"
        "## Acceptance criteria\n"
        "- All tests pass.\n"
        "- Test coverage addresses all major code paths.\n"
        "- The test command exits successfully (e.g. `cargo test`, `pytest`, etc.).\n\n"
        + marker
    )


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def kebab(name: str) -> str:
    """Convert a section name to a kebab-case stage ID."""
    name = name.lower()
    name = re.sub(r"[^a-z0-9\s-]", "", name)
    name = re.sub(r"[\s]+", "-", name.strip())
    return name[:40]


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    args = parse_args()

    if not args.spec.exists():
        print(f"Error: spec file not found: {args.spec}", file=sys.stderr)
        sys.exit(1)

    text = args.spec.read_text(encoding="utf-8")
    lines = text.splitlines()

    project_name = extract_project_name(lines)
    rules        = parse_rules(lines)
    sections     = parse_sections(lines)
    overview     = parse_overview(lines)
    testing_txt  = parse_testing_requirements(lines)

    # Build stages
    stages = []

    # Foundation
    stages.append({
        "id":      "foundation-setup",
        "phase":   "foundation",
        "summary": f"Scaffold {project_name}, set up deps and shared infrastructure",
        "prompt":  make_foundation_prompt(project_name, overview, rules),
    })

    # Implementation (one per section)
    for i, section in enumerate(sections, 1):
        stages.append({
            "id":      f"impl-{kebab(section['name'])}",
            "phase":   "implementation",
            "summary": f"Implement: {section['name']}",
            "prompt":  make_implementation_prompt(project_name, section, i, len(sections)),
        })

    # Testing
    stages.append({
        "id":      "test-suite",
        "phase":   "testing",
        "summary": "Write and run the full test suite",
        "prompt":  make_testing_prompt(project_name, testing_txt),
    })

    config = {
        "name":         project_name,
        "global_rules": rules,
        "interface":    args.interface,
        "provider":     args.provider,
        "state_path":   args.state_path,
        "stages":       stages,
    }
    # Only include optional fields if explicitly set — cleaner JSON.
    if args.model:
        config["model"] = args.model
    if args.api_base_url:
        config["api_base_url"] = args.api_base_url
    if args.api_key_env:
        config["api_key_env"] = args.api_key_env

    args.output.write_text(json.dumps(config, indent=2, ensure_ascii=False), encoding="utf-8")

    print(f"✓ Wrote {len(stages)} stages to {args.output}")
    print(f"  - 1 foundation stage")
    print(f"  - {len(sections)} implementation stage(s)")
    print(f"  - 1 testing stage")
    if rules:
        print(f"  - {len(rules)} global rule(s)")
    else:
        print("  ⚠ No rules section found in spec — add one for best results")
    if not sections:
        print("  ⚠ No sections found — check that your spec has a 'Sections' heading")


if __name__ == "__main__":
    main()
