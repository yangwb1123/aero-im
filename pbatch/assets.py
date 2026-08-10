"""Locate project-local and installed ai-batch-runner assets."""

from __future__ import annotations

import os
import sys
from importlib.metadata import PackageNotFoundError, distribution
from pathlib import Path
from typing import Optional


def _distribution_asset_root() -> Optional[Path]:
    """Locate wheel ``data-files`` without assuming a system/user prefix."""
    try:
        files = distribution("ai-batch-runner").files or ()
    except PackageNotFoundError:
        return None
    suffix = "share/ai-batch-runner/pi-batch.yaml"
    for item in files:
        if str(item).replace("\\", "/").endswith(suffix):
            return Path(item.locate()).resolve().parent
    return None


def _discover_tool_root() -> Path:
    """Find the source checkout or installed root of distributed assets."""
    override = os.environ.get("PBATCH_TOOL_ROOT", "").strip()
    if override:
        return Path(override).expanduser().resolve()
    source = Path(__file__).resolve().parent.parent
    installed = Path(sys.prefix) / "share" / "ai-batch-runner"
    markers = ("scripts", "ui-specs", "backend-specs", "examples", "evals")
    for candidate in (source, installed, _distribution_asset_root()):
        if candidate and all((candidate / marker).exists() for marker in markers):
            return candidate.resolve()
    return source


TOOL_ROOT = str(_discover_tool_root())
PACKAGE_ROOT = str(Path(__file__).resolve().parent)


def asset_candidates(value: str, cwd: str = "") -> list[Path]:
    """Return project-local, installed-asset, then package candidates."""
    path = Path(value).expanduser()
    if path.is_absolute():
        return [path.resolve()]
    project = Path(cwd or os.getcwd()).resolve() / path
    candidates = [project, Path(TOOL_ROOT) / path, Path(PACKAGE_ROOT).parent / path]
    unique = []
    for candidate in candidates:
        resolved = candidate.resolve()
        if resolved not in unique:
            unique.append(resolved)
    return unique


def resolve_asset_path(value: str, cwd: str = "") -> str:
    """Return the first existing project/bundled asset, or local path."""
    candidates = asset_candidates(value, cwd)
    return str(next((path for path in candidates if path.exists()), candidates[0]))


def is_tool_asset(path: Path) -> bool:
    """Whether a resolved path is confined to a trusted distributed root."""
    resolved = path.resolve()
    for root in (Path(TOOL_ROOT), Path(PACKAGE_ROOT)):
        try:
            resolved.relative_to(root.resolve())
            return True
        except ValueError:
            continue
    return False


def placeholder_values(cwd: str, frontend_root: str, backend_root: str,
                       report_root: str) -> dict[str, str]:
    """Resolve validator placeholders against one target project."""
    workdir = str(Path(cwd or os.getcwd()).resolve())

    def project_path(configured: str) -> str:
        path = Path(configured).expanduser()
        return str((path if path.is_absolute() else Path(workdir) / path).resolve())

    return {
        "{cwd}": workdir,
        "{tool_root}": TOOL_ROOT,
        "{frontend_root}": project_path(frontend_root),
        "{backend_root}": project_path(backend_root),
        "{report_root}": project_path(report_root),
    }


def expand_validator_registry(registry: dict, frontend_root: str,
                              backend_root: str, report_root: str) -> dict:
    """Expand static asset roots while retaining runtime cwd/output tokens."""
    def project_expression(configured: str) -> str:
        path = Path(configured).expanduser()
        if path.is_absolute():
            return str(path.resolve())
        suffix = path.as_posix().strip("/")
        return "{cwd}" if suffix in ("", ".") else f"{{cwd}}/{suffix}"

    replacements = {
        "{tool_root}": TOOL_ROOT,
        "{frontend_root}": project_expression(frontend_root),
        "{backend_root}": project_expression(backend_root),
        "{report_root}": project_expression(report_root),
    }

    def expand(command: str) -> str:
        for marker, value in replacements.items():
            command = command.replace(marker, value)
        return command

    result = {}
    for name, entry in registry.items():
        if isinstance(entry, str):
            result[name] = expand(entry)
        elif isinstance(entry, dict):
            mapped = dict(entry)
            if isinstance(mapped.get("cmd"), str):
                mapped["cmd"] = expand(mapped["cmd"])
            result[name] = mapped
        else:
            result[name] = entry
    return result
