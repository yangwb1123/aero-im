"""Defensive scalar readers for declarative configuration sections."""

from __future__ import annotations

import logging
from typing import Optional

log = logging.getLogger("pi-batch")


def _int_setting(section: dict, key: str, default: int,
                 minimum: Optional[int] = None) -> int:
    value = section.get(key, default)
    try:
        if isinstance(value, bool):
            raise ValueError
        parsed = int(value)
    except (TypeError, ValueError):
        log.warning("Config value '%s' must be an integer; using %d", key, default)
        return default
    if minimum is not None and parsed < minimum:
        log.warning("Config value '%s' must be >= %d; using %d", key, minimum, default)
        return default
    return parsed


def _choice_setting(section: dict, key: str, default: str,
                    choices: tuple[str, ...]) -> str:
    value = str(section.get(key, default)).lower()
    if value in choices:
        return value
    log.warning("Config value '%s' must be one of %s; using %s",
                key, ", ".join(choices), default)
    return default


def _string_setting(section: dict, key: str, default: str,
                    allow_empty: bool = True) -> str:
    value = section.get(key, default)
    if isinstance(value, str) and (allow_empty or value):
        return value
    kind = "a string" if allow_empty else "a non-empty string"
    log.warning("Config value '%s' must be %s; using %s", key, kind, default)
    return default


def _float_setting(section: dict, key: str, default: float,
                   minimum: Optional[float] = None,
                   maximum: Optional[float] = None) -> float:
    value = section.get(key, default)
    try:
        if isinstance(value, bool):
            raise ValueError
        parsed = float(value)
    except (TypeError, ValueError):
        log.warning("Config value '%s' must be a number; using %s", key, default)
        return default
    outside = ((minimum is not None and parsed < minimum) or
               (maximum is not None and parsed > maximum))
    if outside:
        log.warning("Config value '%s' must be within [%s, %s]; using %s",
                    key, minimum, maximum, default)
        return default
    return parsed
