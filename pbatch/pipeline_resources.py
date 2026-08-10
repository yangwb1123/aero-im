"""Resolve trusted resources referenced by bundled pipeline templates."""

from __future__ import annotations

from pathlib import Path

from . import config


def normalize_bundled_resources(data: dict) -> None:
    """Use packaged roles/templates when the target project has no copy."""
    stages = data.get("stages", []) if isinstance(data, dict) else []
    for stage in stages if isinstance(stages, list) else []:
        if not isinstance(stage, dict):
            continue
        for field in ("role_dir", "role_keywords"):
            value = stage.get(field)
            if value and not Path(str(value)).exists():
                resolved = Path(config.resolve_asset_path(str(value)))
                if resolved.exists():
                    stage[field] = str(resolved)
        tasks = stage.get("tasks", [])
        for task in tasks if isinstance(tasks, list) else []:
            if not isinstance(task, dict):
                continue
            value = task.get("prompt_template")
            if value and not Path(str(value)).exists():
                resolved = Path(config.resolve_asset_path(str(value)))
                if resolved.is_file() and not resolved.is_symlink():
                    task["prompt_template"] = str(resolved)
