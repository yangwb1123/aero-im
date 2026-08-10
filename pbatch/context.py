"""Context routing (Context Engineering): load the project knowledge a
task actually needs, instead of dumping the whole repository into the
prompt.

The rules manifest (pi-batch rules/assess) selects WHICH SPECS apply; the
context router selects WHICH PROJECT DOCUMENTS to load — ADRs, domain
glossaries, API contracts, security baselines — matched by file path
patterns and task keywords from docs/agent-context/context-map.yaml:

    routes:
      - when:
          paths: ["src/order/**", "modules/order/**"]
        load: [docs/adr/order-state-machine.md, docs/contracts/order-api.yaml]
      - when:
          task_contains: ["权限", "认证", "登录", "auth"]
        load: [docs/security/authentication.md, docs/security/authorization.md]

Usage:
    pi-batch context "<task>"                         # task-keyword routing
    pi-batch context "<task>" --paths src/order/x.ts   # + path routing
    pi-batch context --paths modules/order/**          # path-only routing
    pi-batch context "<task>" --json
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

from . import config
from .config import log, yaml
from .evidence import combine_outputs
from .relevance import _keyword_hit
from .text_io import read_text_bounded

DEFAULT_MAP = Path("docs/agent-context/context-map.yaml")


def _valid_string_list(value, allow_empty: bool = True) -> bool:
    return isinstance(value, list) and (allow_empty or bool(value)) and all(
        isinstance(item, str) and item.strip() for item in value)


def _safe_relative_path(value: str) -> bool:
    path = Path(value)
    return not path.is_absolute() and ".." not in path.parts and bool(path.parts)


def _one_route_errors(route, label: str) -> list[str]:
    if not isinstance(route, dict):
        return [f"{label} must be a mapping"]
    errors = []
    unknown = sorted(set(route) - {"when", "load"})
    if unknown:
        errors.append(f"{label} unknown field(s): {', '.join(unknown)}")
    when, files = route.get("when"), route.get("load")
    if not isinstance(when, dict):
        return errors + [f"{label}.when must be a mapping"]
    unknown_when = sorted(set(when) - {"paths", "task_contains"})
    if unknown_when:
        errors.append(f"{label}.when unknown field(s): {', '.join(unknown_when)}")
    for key in ("paths", "task_contains"):
        if not _valid_string_list(when.get(key, [])):
            errors.append(f"{label}.when.{key} must be a string list")
    if not when.get("paths") and not when.get("task_contains"):
        errors.append(f"{label}.when needs paths or task_contains")
    if not _valid_string_list(files, allow_empty=False):
        errors.append(f"{label}.load must be a non-empty string list")
    elif not all(_safe_relative_path(item) for item in files):
        errors.append(f"{label}.load paths must stay relative to the project root")
    return errors


def _route_errors(routes) -> list[str]:
    if not isinstance(routes, list):
        return ["routes must be a list"]
    errors = []
    for index, route in enumerate(routes, 1):
        errors.extend(_one_route_errors(route, f"routes[{index}]"))
    return errors


def load_context_map(path: str = "") -> list:
    """Load and validate routes; explicit overrides fail closed."""
    candidates = [Path(path)] if path else [DEFAULT_MAP]
    for candidate in candidates:
        if not candidate.exists():
            if path:
                raise ValueError(f"context map not found: {candidate}")
            continue
        if candidate.is_symlink() or not candidate.is_file():
            message = f"context map is not a regular file: {candidate}"
            if path:
                raise ValueError(message)
            log.warning(message)
            return []
        try:
            data = yaml.safe_load(read_text_bounded(
                candidate, config.INPUT_MAX_BYTES, "context map")) or {}
        except Exception as exc:
            if path:
                raise ValueError(f"invalid context map: {exc}") from exc
            continue
        if not isinstance(data, dict):
            errors = ["top level must be a mapping"]
        else:
            unknown = sorted(set(data) - {"routes"})
            errors = (["unknown top-level field(s): " + ", ".join(unknown)]
                      if unknown else [])
            errors.extend(_route_errors(data.get("routes")))
        if errors:
            if path:
                raise ValueError("invalid context map: " + "; ".join(errors))
            log.warning("default context map invalid: %s", "; ".join(errors))
            return []
        return data["routes"]
    return []


def _path_matches(path: str, pattern: str) -> bool:
    import fnmatch
    norm = path.replace("\\", "/")
    return fnmatch.fnmatch(norm, pattern.replace("\\", "/")) or \
        fnmatch.fnmatch(norm, pattern + "/**")


def _project_root(value: str = "") -> Path:
    root = Path(value or Path.cwd()).resolve()
    if not root.is_dir():
        raise ValueError(f"context project root is not a directory: {root}")
    return root


def _context_target(value: str, root: Path) -> Path:
    if not _safe_relative_path(value):
        raise ValueError(f"unsafe context document path: {value}")
    relative = Path(value)
    cursor = root
    for part in relative.parts:
        cursor /= part
        if cursor.is_symlink():
            raise ValueError(f"context document path contains a symlink: {value}")
    target = (root / relative).resolve()
    try:
        target.relative_to(root)
    except ValueError as exc:
        raise ValueError(f"context document escapes project root: {value}") from exc
    return target


def match_context(text: str, paths: Optional[list] = None,
                  context_map: Optional[list] = None,
                  project_root: str = "") -> dict:
    """(matches, files, missing) for the given task text and touched paths."""
    routes = context_map if context_map is not None else load_context_map()
    root = _project_root(project_root)
    lowered = (text or "").lower()
    files, missing, matched_routes = [], [], 0
    for route in routes:
        when = route.get("when", {}) if isinstance(route.get("when"), dict) else {}
        path_patterns = when.get("paths", [])
        terms = when.get("task_contains", [])
        path_hit = bool(paths) and any(
            _path_matches(p, str(pattern)) for p in paths for pattern in path_patterns)
        term_hit = any(_keyword_hit(lowered, str(t)) for t in terms)
        if not (path_hit or term_hit):
            continue
        matched_routes += 1
        for file in route.get("load", []):
            shown = Path(str(file)).as_posix()
            target = _context_target(shown, root)
            if target.is_file():
                files.append(shown)
            else:
                missing.append(f"{file} (route matched, file missing)")
    # dedupe preserving order
    seen, unique = set(), []
    for f in files:
        if f not in seen:
            seen.add(f)
            unique.append(f)
    return {"files": unique, "missing": missing,
            "routes_matched": matched_routes > 0}


def format_context(manifest: dict) -> str:
    """Human-readable context manifest for prompt injection."""
    lines = ["## Project context to load (context router)"]
    if not manifest["files"]:
        if manifest["missing"]:
            lines.append("- matched routes, but the referenced documents do "
                         "not exist yet:")
            for missing in manifest["missing"]:
                lines.append(f"    - MISSING: {missing}")
        else:
            lines.append("- (none matched — load nothing extra)")
        return "\n".join(lines)
    for file in manifest["files"]:
        lines.append(f"- {file}")
    for missing in manifest["missing"]:
        lines.append(f"- MISSING: {missing}")
    return "\n".join(lines)


def load_context_content(manifest: dict, maximum: int = 0,
                         project_root: str = "") -> str:
    """Load matched project documents as bounded, fenced prompt evidence."""
    root = _project_root(project_root)
    files = [_context_target(str(value), root)
             for value in manifest.get("files", [])]
    return combine_outputs(files, maximum)


def context_main(argv: list) -> int:
    """`pi-batch context "<task>" [--paths P1,P2] [--json]`."""
    import argparse
    parser = argparse.ArgumentParser(
        prog="pi-batch.py context",
        description="Route the project documents a task needs (Context "
                    "Engineering; docs/agent-context/context-map.yaml).")
    parser.add_argument("task", nargs="*", default=[""], help="task prompt text")
    parser.add_argument("--paths", default="",
                        help="comma-separated touched file paths (glob ok)")
    parser.add_argument("--map", default="", help="context map YAML override")
    parser.add_argument("--root", default=".",
                        help="project root that context documents must stay within")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--content", action="store_true",
                        help="include bounded, fenced document contents")
    parser.add_argument("--max-bytes", type=int, default=0,
                        help="content budget (default: evidence.max_bytes)")
    args = parser.parse_args(argv)
    if args.max_bytes < 0:
        parser.error("--max-bytes must be >= 0")
    text = " ".join(args.task)
    paths = [p.strip() for p in args.paths.split(",") if p.strip()]
    try:
        default_map = Path(args.root) / DEFAULT_MAP
        routes = (load_context_map(args.map) if args.map else
                  load_context_map(str(default_map)) if default_map.exists() else [])
    except ValueError as exc:
        parser.error(str(exc))
    try:
        manifest = match_context(text, paths or None, routes, args.root)
        if args.content:
            manifest["content"] = load_context_content(
                manifest, args.max_bytes, args.root)
    except ValueError as exc:
        parser.error(str(exc))
    if args.json:
        print(json.dumps(manifest, ensure_ascii=False, indent=2))
        return 0
    print(format_context(manifest))
    if args.content and manifest.get("content"):
        print("\n" + manifest["content"])
    if manifest["missing"]:
        for item in manifest["missing"]:
            log.warning("context route references a missing file: %s", item)
    return 0
