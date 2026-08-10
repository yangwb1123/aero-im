"""Rule-registry validation and command-line adapter for rule matching."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Optional

from . import config
from .classifier import classify_text
from .config import log
from .rule_matcher import (LEVELS, TIER_ORDER, _default_registry_paths,
                           _reconcile_cli, check_registry, domain_for,
                           format_llm_prompt, format_manifest, load_registry,
                           match_rules, summarize_task)
from .text_io import read_text_bounded

CHECK_ROOT = Path(config.TOOL_ROOT)


def _check_suppressions(rule_id: str, rule: dict, violations: list) -> None:
    """Validate the explicit activation-suppression vocabulary."""
    suppressions = rule.get("suppress_on")
    if suppressions is None:
        return
    if not isinstance(suppressions, list) or not suppressions:
        violations.append(
            f"rule '{rule_id}': suppress_on must be a non-empty string list")
        return
    for signal in suppressions:
        if signal not in TIER_ORDER and signal not in ("high", "low"):
            violations.append(
                f"rule '{rule_id}': suppress_on '{signal}' must be a tier "
                "or high/low")


def _check_self_reference(rule_id: str, rule: dict, files: list,
                          template: str, registry_paths: set,
                          violations: list) -> None:
    """Compatibility helper for callers that validate one rule directly."""
    candidates = list(files)
    if template:
        candidates.extend(template.replace("{profile}", str(profile))
                          for profile in rule.get("profiles", []) or [])
    for value in candidates:
        if str(Path(str(value)).resolve()) in registry_paths:
            violations.append(
                f"rule '{rule_id}': files entry '{value}' resolves to the "
                "registry file itself (self-referential rule)")


def _check_rule(rule_id: str, rule, violations: list,
                registry_paths: Optional[set] = None) -> None:
    """Validate one rule using the same fail-closed public schema."""
    paths = list(registry_paths) if registry_paths else None
    violations.extend(check_registry(
        {"rules": {rule_id: rule}}, registry_paths=paths))


def _registry_path(domain: str) -> Optional[Path]:
    return next((path.resolve() for path in _default_registry_paths(domain)
                 if path.is_file()), None)


def _check_registries_cli() -> None:
    """Validate all three bundled registries; missing/empty is a failure."""
    for domain in ("frontend_ui", "backend", "product"):
        path = _registry_path(domain)
        if path is None:
            log.error("[%s] registry missing; refusing OK", domain)
            sys.exit(1)
        try:
            registry = load_registry(domain=domain)
        except ValueError as exc:
            log.error("[%s] registry invalid: %s", domain, exc)
            sys.exit(1)
        if not registry.get("rules"):
            log.error("[%s] registry empty; refusing OK", domain)
            sys.exit(1)
        violations = check_registry(
            registry, asset_base=path.parent, registry_paths=[str(path)])
        if violations:
            log.error("[%s] registry invalid:", domain)
            for violation in violations:
                log.error("  %s", violation)
            sys.exit(1)
        log.info("[%s] registry: OK", domain)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pi-batch.py rules",
        description="Match domain spec rules and reconcile an independent "
                    "LLM selection (two-sided check).")
    parser.add_argument("task", nargs="*", default=[], help="task prompt text")
    parser.add_argument("--file", default="", help="read task text from FILE")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--summary", action="store_true",
                        help="print the compressed requirement (LLM input)")
    parser.add_argument("--llm-prompt", default="",
                        help="write the two-sided-check prompt to FILE")
    parser.add_argument("--llm-json", default="",
                        help='LLM selection JSON {"apply":[],"skip":[]}')
    parser.add_argument("--registry", default="", help="rule registry YAML override")
    parser.add_argument("--check", action="store_true",
                        help="validate frontend/backend/product registries")
    return parser


def _task_text(args, parser: argparse.ArgumentParser) -> str:
    if args.file and args.task:
        parser.error("use either positional task text or --file, not both")
    if args.file:
        try:
            text = read_text_bounded(
                Path(args.file), config.INPUT_MAX_BYTES, "rules source")
        except (OSError, ValueError) as exc:
            parser.error(str(exc))
    else:
        text = " ".join(args.task)
    if not text.strip():
        parser.error("Provide a task description (positional or --file)")
    return text


def _registry(text: str, override: str, parser: argparse.ArgumentParser):
    classification = classify_text(text)
    domain = domain_for(text, classification)
    if not override:
        return None
    try:
        registry = load_registry(override, domain=domain)
    except ValueError as exc:
        parser.error(str(exc))
    target = Path(override).resolve()
    errors = check_registry(
        registry, asset_base=target.parent, registry_paths=[str(target)])
    if errors:
        parser.error("invalid custom registry: " + "; ".join(errors[:5]))
    return registry


def rules_main(argv: list) -> None:
    """Run ``pi-batch rules`` with bounded input and strict registries."""
    parser = _parser()
    args = parser.parse_args(argv)
    if args.check:
        _check_registries_cli()
        return
    text = _task_text(args, parser)
    registry = _registry(text, args.registry, parser)
    if args.llm_json:
        _reconcile_cli(text, args.llm_json, registry, args.json)
        return
    if args.summary:
        print(summarize_task(text, registry))
        return
    if args.llm_prompt:
        target = Path(args.llm_prompt)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(format_llm_prompt(text, registry), encoding="utf-8")
        print(f"LLM check prompt written to {target}")
        return
    matched = match_rules(text, registry=registry)
    if args.json:
        print(json.dumps(matched, ensure_ascii=False, indent=2))
    else:
        print(format_manifest(matched))
