"""Trusted path resolution for repository campaigns."""

from __future__ import annotations

from pathlib import Path

from . import config
from .campaign_models import CampaignSettings


def validate_paths(root: Path, settings: CampaignSettings) -> None:
    """Validate writable paths and resolve the implementation template."""
    for value in (settings.output_dir, settings.state_file, settings.summary_file,
                  settings.worktree_root):
        inside(root, settings.path(root, value))
    local_template = settings.path(root, settings.pipeline_template)
    template = (local_template if local_template.exists() else
                Path(config.resolve_asset_path(settings.pipeline_template, str(root))))
    if not inside_or_tool_asset(root, template):
        raise ValueError(f"campaign pipeline template escapes repository: {template}")
    if not template.is_file() or template.is_symlink():
        raise ValueError(f"campaign pipeline template is missing or unsafe: {template}")
    settings.pipeline_template = str(template)


def campaign_config_path(root: Path, value: str) -> Path:
    """Prefer a repository config, then a trusted bundled default."""
    requested = Path(value)
    if requested.is_absolute():
        return inside(root, requested)
    local = inside(root, root / requested)
    if local.exists():
        return local
    bundled = Path(config.resolve_asset_path(value, str(root)))
    return bundled if config.is_tool_asset(bundled) else local


def inside_or_tool_asset(root: Path, path: Path) -> bool:
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return config.is_tool_asset(path)


def inside(root: Path, path: Path) -> Path:
    """Resolve a path and reject traversal outside the target repository."""
    resolved = path.resolve()
    try:
        resolved.relative_to(root.resolve())
    except ValueError as exc:
        raise ValueError(f"campaign path escapes repository: {path}") from exc
    return resolved
