"""Strict schema and asset validation for rule registries."""

from __future__ import annotations

import math
import sys
from pathlib import Path
from typing import Optional

from . import config
from .config import log

_TIERS = {"demo", "standard", "production"}
_LIST_SECTIONS = ("scale", "risk", "page_types", "use_case_types", "signals")
_RULE_LIST_FIELDS = ("files", "profiles", "page_types", "risk")
_RULE_BOOL_FIELDS = ("required", "required_when_risk")
_RULE_FIELDS = {"description", "min_tier", "files", "files_template",
                "profiles", "page_types", "risk", *_RULE_BOOL_FIELDS}


def _is_string_list(value) -> bool:
    return isinstance(value, list) and bool(value) and all(
        isinstance(item, str) and bool(item.strip()) for item in value)


def _asset_exists(value: str, asset_base: Optional[Path]) -> bool:
    path = Path(value)
    candidates = [path] if path.is_absolute() else []
    if asset_base is not None:
        candidates.append(asset_base / path)
    candidates.append(Path(config.resolve_asset_path(value)))
    return any(candidate.is_file() for candidate in candidates)


def _rule_errors(rule_id: str, rule, asset_base: Optional[Path]) -> list[str]:
    if not isinstance(rule, dict):
        return [f"rule '{rule_id}': must be a mapping"]
    errors = []
    unknown = sorted(set(rule) - _RULE_FIELDS)
    if unknown:
        errors.append(f"rule '{rule_id}': unknown field(s): {', '.join(unknown)}")
    if not isinstance(rule.get("description"), str) or not rule["description"].strip():
        errors.append(f"rule '{rule_id}': missing 'description'")
    if rule.get("min_tier") not in _TIERS:
        errors.append(f"rule '{rule_id}': invalid min_tier {rule.get('min_tier')!r}")
    for key in _RULE_LIST_FIELDS:
        if key in rule and not _is_string_list(rule[key]):
            errors.append(f"rule '{rule_id}': {key} must be a non-empty string list")
    for key in _RULE_BOOL_FIELDS:
        if key in rule and not isinstance(rule[key], bool):
            errors.append(f"rule '{rule_id}': {key} must be boolean")
    template = rule.get("files_template")
    if template is not None and not isinstance(template, str):
        errors.append(f"rule '{rule_id}': files_template must be a string")
    if not rule.get("files") and not template:
        errors.append(f"rule '{rule_id}': missing 'files' or 'files_template'")
    errors.extend(_rule_asset_errors(rule_id, rule, asset_base))
    return errors


def _rule_asset_errors(rule_id: str, rule: dict,
                       asset_base: Optional[Path]) -> list[str]:
    errors = [f"rule '{rule_id}': missing file {value}"
              for value in rule.get("files", [])
              if isinstance(value, str) and not _asset_exists(value, asset_base)]
    template = rule.get("files_template")
    if not isinstance(template, str):
        return errors
    for profile in rule.get("profiles", []):
        target = template.replace("{profile}", profile)
        if not _asset_exists(target, asset_base):
            errors.append(f"rule '{rule_id}': missing profile file {target}")
    return errors


def check_registry(registry: dict, asset_base: Optional[Path] = None) -> list[str]:
    """Return all type, rule, and referenced-asset violations."""
    if not isinstance(registry, dict):
        return ["registry must be a mapping"]
    rules = registry.get("rules")
    if not isinstance(rules, dict) or not rules:
        return ["'rules' section must be a non-empty mapping"]
    errors = []
    for section in _LIST_SECTIONS:
        value = registry.get(section, {})
        if not isinstance(value, dict):
            errors.append(f"section '{section}' must be a mapping")
        elif not all(_is_string_list(terms) for terms in value.values()):
            errors.append(f"section '{section}' values must be non-empty string lists")
    weights = registry.get("signal_weights", {})
    if not isinstance(weights, dict) or not all(
            isinstance(weight, (int, float)) and not isinstance(weight, bool)
            and math.isfinite(weight) and weight >= 0
            for weight in weights.values()):
        errors.append("section 'signal_weights' must map names to non-negative numbers")
    for rule_id, rule in rules.items():
        errors.extend(_rule_errors(str(rule_id), rule, asset_base))
    return errors


def check_registries_cli() -> None:
    """Validate both bundled domain registries and exit nonzero on errors."""
    from .rule_matcher import load_registry
    for domain, relative in (("frontend", "ui-specs/rules.yaml"),
                             ("backend", "backend-specs/rules.yaml")):
        resolved = Path(config.resolve_asset_path(relative))
        if not resolved.is_file():
            log.error("registry not found: %s", relative)
            sys.exit(1)
        try:
            registry = load_registry(str(resolved), domain=domain)
        except ValueError as exc:
            log.error("%s registry: %s", domain, exc)
            sys.exit(1)
        violations = check_registry(registry, resolved.parent)
        if violations:
            for item in violations:
                log.error("%s registry: %s", domain, item)
            sys.exit(1)
        log.info("%s registry: OK", domain)
