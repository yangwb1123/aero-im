"""Defensive parsing for the optional device execution fabric."""

from __future__ import annotations

import logging
from typing import NamedTuple


log = logging.getLogger("pi-batch")


class DeviceFabricConfig(NamedTuple):
    enabled: bool
    mode: str
    lan_scan: bool
    benchmark: bool
    auto_install: bool
    approval_required: bool
    static: tuple[dict, ...]
    clusters: dict[str, list[str]]


def load_device_fabric(section: dict) -> DeviceFabricConfig:
    """Parse independent, off-by-default fabric controls."""
    inventory = section.get("inventory")
    inventory = inventory if isinstance(inventory, dict) else {}
    static = inventory.get("static")
    static = static if isinstance(static, list) else []
    clusters = section.get("clusters")
    clusters = clusters if isinstance(clusters, dict) else {}
    return DeviceFabricConfig(
        enabled=_bool_setting(section, "enabled", False),
        mode=_mode_setting(section),
        lan_scan=_bool_setting(section, "lan_active_scan", False),
        benchmark=_bool_setting(section, "benchmark", False),
        auto_install=_bool_setting(section, "auto_install", False),
        approval_required=_bool_setting(
            section, "approval_required_for_new_device", True),
        static=tuple(entry for entry in static if isinstance(entry, dict)),
        clusters={str(name): [str(item) for item in members]
                  for name, members in clusters.items()
                  if isinstance(members, list)},
    )


def _bool_setting(section: dict, key: str, default: bool) -> bool:
    value = section.get(key, default)
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in ("true", "yes", "on", "1"):
            return True
        if normalized in ("false", "no", "off", "0"):
            return False
    log.warning("Config value '%s' must be a boolean; using %s", key, default)
    return default


def _mode_setting(section: dict) -> str:
    choices = ("off", "inventory", "observe", "execute", "migrate", "federate")
    value = str(section.get("mode", "off")).lower()
    if value in choices:
        return value
    log.warning("Config value 'mode' must be one of %s; using off",
                ", ".join(choices))
    return "off"
