"""Formatting helpers for deterministic rule manifests."""

from __future__ import annotations

from pathlib import Path

from . import config


def manifest_rule_lines(item: dict) -> list[str]:
    """Format one rule and resolve any packaged specification paths."""
    marker = "REQUIRED" if item["required"] else "optional"
    source = ""
    if item.get("provenance", "algorithm") != "algorithm":
        source = f" [{item['provenance']}]"
    lines = [f"- [{marker}]{source} {item['id']} ({item['tier']}): "
             f"{item['description']}"]
    for file in item["files"]:
        path = Path(str(file))
        shown = (str(path) if path.is_absolute() or path.exists()
                 else config.resolve_asset_path(str(file)))
        lines.append(f"    {shown}")
    return lines
