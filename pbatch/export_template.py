"""`pi-batch export-template [--specs SPECS] [-o OUT]` — 分发智能落地。

把规范资产打包为可分享、可移植的模板目录（Distribution Intelligence：
分发物必须可复现、自验证、可接手——README + 校验 + 版本清单）。

默认导出设计智能规范包（前端 6 + 后端 8 + 哲学 + 检查器）。
`--specs` 可替换默认规范集，`--no-checkers` 排除门禁脚本，`--all`
导出全部规范资产。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

from . import config

ROOT = Path(config.TOOL_ROOT)

_DEFAULT_SPECS = [
    "ui-specs/design-intelligence",
    "backend-specs/design-intelligence",
    "docs/ENGINEERING_PHILOSOPHY.md",
]
_DEFAULT_CHECKERS = [
    "scripts/check-design-intelligence.py",
    "scripts/check-backend-experience.py",
    "scripts/check-knowledge-freshness.py",
]
_PIPELINE_TEMPLATES = [
    "examples/ui-generation-pipeline.yaml",
    "examples/backend-implementation-pipeline.yaml",
]


def _copy(src: Path, dest_root: Path) -> int:
    try:
        relative = src.resolve().relative_to(ROOT.resolve())
    except ValueError:
        relative = Path(src.name)
    target = dest_root / relative
    if src.is_dir():
        shutil.copytree(src, target, dirs_exist_ok=True)
        return sum(1 for _ in target.rglob("*") if _.is_file())
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, target)
    return 1


def _source_label(source: Path) -> str:
    """Stable manifest label, relative to the tool root when possible."""
    try:
        return str(source.resolve().relative_to(ROOT.resolve()))
    except ValueError:
        return source.name


def _is_within(path: Path, parent: Path) -> bool:
    """Whether path resolves inside parent (including parent itself)."""
    try:
        path.resolve().relative_to(parent.resolve())
        return True
    except ValueError:
        return False


def _validate_export(sources: list[Path], out: Path) -> None:
    """Reject incomplete or ambiguous exports before writing anything."""
    missing = [str(source) for source in sources if not source.exists()]
    if missing:
        raise FileNotFoundError("missing export assets: " + ", ".join(missing))
    nested = [str(source) for source in sources
              if source.is_dir() and _is_within(out, source)]
    if nested:
        raise ValueError("output directory is inside an exported asset: " + nested[0])
    if out.exists() and any(out.iterdir()):
        raise ValueError(f"output directory is not empty: {out}")


def _hash_exported_files(out: Path) -> dict[str, str]:
    """Content hashes make the exported pack independently verifiable."""
    hashes = {}
    for path in sorted(item for item in out.rglob("*") if item.is_file()):
        if path.name == "VERSION.json":
            continue
        hashes[str(path.relative_to(out))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return hashes


def _readme_text(manifest: dict, checkers: list[Path]) -> str:
    """Generate handoff instructions that match the files actually exported."""
    commands = [f"python {_source_label(path)} --selfcheck" for path in checkers]
    if commands:
        validation = "```bash\n" + "\n".join(commands) + "\n```"
    else:
        validation = "此包未包含检查器；请使用接收项目自己的工程门禁验证。"
    return (
        "# Design Intelligence Pack\n\n"
        f"导出时间: {manifest['exported_at']}\n"
        f"规范 {len(manifest['specs'])} 项 / 检查器 {len(checkers)} / "
        f"流水线 {len(manifest['pipelines'])}\n\n"
        f"## 校验\n\n{validation}\n")


def export_template(specs: list[Path], checkers: list[Path],
                    pipelines: list[Path], out: Path) -> dict:
    sources = specs + checkers + pipelines
    _validate_export(sources, out)
    out.mkdir(parents=True, exist_ok=True)
    manifest = {
        "exported_at": datetime.now(timezone.utc).isoformat(),
        "source": "ai-batch-runner",
        "specs": [_source_label(s) for s in specs],
        "checkers": [_source_label(c) for c in checkers],
        "pipelines": [_source_label(p) for p in pipelines],
        "files": {},
    }
    total = 0
    for src in sources:
        count = _copy(src, out)
        manifest["files"][_source_label(src)] = count
        total += count
    readme = out / "README.md"
    readme.write_text(_readme_text(manifest, checkers), encoding="utf-8")
    manifest["sha256"] = _hash_exported_files(out)
    (out / "VERSION.json").write_text(
        json.dumps(manifest, indent=2), encoding="utf-8")
    return {"files": total, "manifest": str(out / "VERSION.json")}


def export_main(argv: list | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pi-batch.py export-template",
                                     description=__doc__)
    parser.add_argument("--all", action="store_true",
                        help="export every spec asset (ui/backend/docs)")
    parser.add_argument("--specs", action="append", default=[], metavar="PATHS",
                        help="comma-separated spec paths; repeatable; replaces defaults")
    checker_group = parser.add_mutually_exclusive_group()
    checker_group.add_argument("--checkers", dest="checkers", action="store_true",
                               help="include checker scripts (default)")
    checker_group.add_argument("--no-checkers", dest="checkers", action="store_false",
                               help="exclude checker scripts")
    parser.set_defaults(checkers=True)
    parser.add_argument("--pipelines", action="store_true",
                        help="include pipeline templates")
    parser.add_argument("-o", "--output", default="dist/design-intelligence-pack",
                        help="output directory")
    args = parser.parse_args(argv)
    if args.all and args.specs:
        parser.error("--all and --specs cannot be combined")
    specs = [ROOT / p for p in _DEFAULT_SPECS]
    if args.all:
        specs = [ROOT / "ui-specs", ROOT / "backend-specs",
                 ROOT / "product-specs",
                 ROOT / "docs" / "ENGINEERING_PHILOSOPHY.md"]
    elif args.specs:
        values = [item.strip() for group in args.specs
                  for item in group.split(",") if item.strip()]
        specs = [Path(value).expanduser() if Path(value).is_absolute()
                 else ROOT / value for value in values]
    checkers = [ROOT / p for p in _DEFAULT_CHECKERS] if args.checkers else []
    pipelines = [ROOT / p for p in _PIPELINE_TEMPLATES] if args.pipelines else []
    out = Path(args.output).expanduser()
    if not out.is_absolute():
        out = Path.cwd() / out
    try:
        report = export_template(specs, checkers, pipelines, out)
    except (OSError, ValueError) as exc:
        print(f"export-template: {exc}", file=sys.stderr)
        return 2
    print(f"exported {report['files']} files -> {out}")
    print(f"manifest: {report['manifest']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(export_main())
