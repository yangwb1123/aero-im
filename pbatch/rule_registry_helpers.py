"""Compatibility validation for governance-aware rule registries."""

from __future__ import annotations

from pathlib import Path
from typing import Optional

from .rule_registry import check_registry as _validate_core

_LEVELS = ("invariant", "contract", "policy", "heuristic", "suggestion")
_TIERS = {"demo", "standard", "production"}
_EXTENSION_FIELDS = {"level", "suppress_on"}


def _extension_errors(registry: dict) -> list[str]:
    errors = []
    for rule_id, rule in (registry.get("rules") or {}).items():
        if not isinstance(rule, dict):
            continue
        level = rule.get("level")
        if level is not None and level not in _LEVELS:
            errors.append(
                f"rule '{rule_id}': invalid level {level!r} "
                f"(must be one of {_LEVELS})")
        errors.extend(_suppression_errors(str(rule_id), rule.get("suppress_on")))
    return errors


def _suppression_errors(rule_id: str, suppressions) -> list[str]:
    if suppressions is None:
        return []
    if (not isinstance(suppressions, list) or not suppressions
            or not all(isinstance(item, str) and item.strip()
                       for item in suppressions)):
        return [f"rule '{rule_id}': suppress_on must be a non-empty string list"]
    return [f"rule '{rule_id}': suppress_on '{signal}' must be a tier or high/low"
            for signal in suppressions
            if signal not in _TIERS and signal not in ("high", "low")]


def _rule_files(rule: dict) -> list:
    files = list(rule.get("files") or [])
    template = rule.get("files_template")
    if isinstance(template, str):
        files.extend(template.replace("{profile}", str(profile))
                     for profile in rule.get("profiles", []) or [])
    return files


def _resolved_candidates(value, asset_base: Optional[Path]) -> set[str]:
    path = Path(str(value))
    candidates = {str(path.resolve())}
    if not path.is_absolute() and asset_base is not None:
        candidates.add(str((asset_base / path).resolve()))
    return candidates


def _self_reference_errors(registry: dict, registry_paths: list,
                           asset_base: Optional[Path]) -> list[str]:
    resolved = {str(Path(path).resolve()) for path in registry_paths}
    errors = []
    for rule_id, rule in (registry.get("rules") or {}).items():
        if not isinstance(rule, dict):
            continue
        for value in _rule_files(rule):
            if _resolved_candidates(value, asset_base) & resolved:
                errors.append(
                    f"rule '{rule_id}': files entry '{value}' resolves to the "
                    "registry file itself (self-referential rule)")
    return errors


def _without_extensions(registry: dict) -> dict:
    normalized = dict(registry)
    rules = registry.get("rules")
    if isinstance(rules, dict):
        normalized["rules"] = {
            rule_id: ({key: value for key, value in rule.items()
                       if key not in _EXTENSION_FIELDS}
                      if isinstance(rule, dict) else rule)
            for rule_id, rule in rules.items()
        }
    return normalized


def validate_registry(registry: dict, asset_base: Optional[Path] = None,
                      registry_paths: Optional[list] = None) -> list[str]:
    """Validate core schema, governance extensions and self references."""
    if registry_paths is None and isinstance(asset_base, (list, tuple, set)):
        registry_paths, asset_base = list(asset_base), None
    if not isinstance(registry, dict):
        return _validate_core(registry, asset_base)
    errors = _validate_core(_without_extensions(registry), asset_base)
    errors.extend(_extension_errors(registry))
    if registry_paths:
        errors.extend(_self_reference_errors(
            registry, list(registry_paths), asset_base))
    return errors
