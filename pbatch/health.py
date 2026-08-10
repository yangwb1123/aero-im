"""Read-only project health report for ``pi-batch health``."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Optional

from . import config

ROOT = Path(config.TOOL_ROOT)
ASSET_ROOT = Path(config.TOOL_ROOT)
SKIP = ".git,.dart_tool,test,build,vendor,checks"


def _resolved_root(root: Optional[Path]) -> Path:
    return Path(root or ROOT).resolve()


def _count_tests(root: Optional[Path] = None) -> int:
    """Count test functions without executing them."""
    total = 0
    for path in (_resolved_root(root) / "tests").rglob("test_*.py"):
        if ".dart_tool" in path.parts:
            continue
        try:
            source = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        total += len(re.findall(r"^def test_|^    def test_", source, re.M))
    return total


def _checker_violations(name: str, root: Optional[Path] = None) -> int:
    """Run one JSON checker; any execution/protocol error is unknown (-1)."""
    script = ASSET_ROOT / "scripts" / f"{name}.py"
    if not script.is_file():
        return -1
    cmd = [sys.executable, str(script), "--dir", str(_resolved_root(root)),
           "--json", "--skip", SKIP]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return -1
    if result.returncode not in (0, 1):
        return -1
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        return -1
    total = payload.get("total", -1) if isinstance(payload, dict) else -1
    return total if isinstance(total, int) and total >= 0 else -1


def _quality_violations(root: Optional[Path] = None) -> int:
    script = ASSET_ROOT / "quality.py"
    if not script.is_file():
        return -1
    try:
        result = subprocess.run(
            [sys.executable, str(script), str(_resolved_root(root))],
            capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return -1
    match = re.search(r"(\d+) violation", result.stdout)
    if match:
        return int(match.group(1))
    return 0 if result.returncode == 0 and "quality: OK" in result.stdout else -1


def _spec_counts(root: Optional[Path] = None) -> dict:
    project_root = _resolved_root(root)
    ui = len(list((ASSET_ROOT / "ui-specs").rglob("*.md")))
    backend = len(list((ASSET_ROOT / "backend-specs").rglob("*.md")))
    drafts = len(list((project_root / "docs" / "rules" / "drafts").glob("*.yaml")))
    rounds = 0
    state = project_root / "docs" / "advance" / "state.jsonl"
    if state.exists():
        try:
            rounds = sum(1 for _ in state.open(encoding="utf-8"))
        except OSError:
            rounds = 0
    return {"ui_specs": ui, "backend_specs": backend,
            "rule_drafts": drafts, "advance_rounds": rounds}


def _line_count(path: Path) -> int:
    try:
        return sum(1 for _ in path.open(encoding="utf-8")) if path.exists() else 0
    except OSError:
        return 0


def _governance_counts(root: Optional[Path] = None) -> dict:
    """Device-fabric and governance-state counters (best-effort, read-only)."""
    project_root = _resolved_root(root)
    state_dir = project_root / ".pi-batch" / "devices"
    counts = {
        "runners": _line_count(state_dir / "runners.jsonl"),
        "events": _line_count(state_dir / "events.jsonl"),
        "drafts": len(list((project_root / "docs/rules/drafts").glob("*.yaml"))),
        "exceptions": len(list((project_root / "docs/rules/exceptions").glob("*.yaml"))),
        "promoted": len(list((project_root / "docs/rules/promoted").glob("*.yaml"))),
        "truth_invalidations": 0,
    }
    truth = project_root / ".pi-batch" / "truth.jsonl"
    if truth.exists():
        try:
            counts["truth_invalidations"] = sum(
                1 for line in truth.open(encoding="utf-8")
                if '"invalidate"' in line)
        except OSError:
            pass
    return counts


def _health_score(checkers: dict, quality: int, tests: int) -> dict:
    """Agent OS health score (0-100) with fail-closed unknown probes."""
    quality_score = 0 if quality < 0 else max(0, 25 - quality * 5)
    gate_score = 25.0
    for count in checkers.values():
        gate_score -= 8 if count < 0 else max(0, count * 8)
    gate_score = max(0, gate_score)
    arch_score, cycles = 20.0, -1
    try:
        from .hypergraph import extract_module_graph
        graph = extract_module_graph(str(Path(__file__).resolve().parent))
        cycles = len(graph.depends_on_cycles())
        arch_score = max(0, arch_score - cycles * 5)
    except Exception:
        arch_score = 0
    gov_score, overruns = 15.0, -1
    try:
        from .capabilities import registry_check
        overruns = registry_check()["stats"]["deps_over_budget"]
        gov_score = max(0, gov_score - overruns * 3)
    except Exception:
        gov_score = 0
    test_score = min(15, tests / 800 * 15)
    score = quality_score + gate_score + arch_score + gov_score + test_score
    breakdown = {
        "code_quality": quality_score, "gates": round(gate_score, 1),
        "architecture_cycles": cycles, "architecture": arch_score,
        "budget_overruns": overruns, "governance_budget": gov_score,
        "tests_collected": tests, "test_scale": round(test_score, 1),
    }
    return {"score": round(max(0, min(100, score)), 1),
            "breakdown": breakdown,
            "grade": ("A" if score >= 90 else "B" if score >= 75
                      else "C" if score >= 60 else "D")}


def health_report(root: Optional[Path] = None) -> dict:
    """Gather all health signals; explicit roots propagate to every probe."""
    explicit = root is not None
    project_root = _resolved_root(root)
    tests = _count_tests(project_root) if explicit else _count_tests()
    checkers = {
        name: (_checker_violations(script, project_root) if explicit
               else _checker_violations(script))
        for name, script in {
            "designintelligence": "check-design-intelligence",
            "knowledge": "check-knowledge-freshness",
            "backendquality": "check-backend-quality",
        }.items()
    }
    quality = (_quality_violations(project_root) if explicit
               else _quality_violations())
    specs = _spec_counts(project_root) if explicit else _spec_counts()
    governance = (_governance_counts(project_root) if explicit
                  else _governance_counts())
    return {
        "governance": governance,
        "score": _health_score(checkers, quality, tests),
        "tests": tests,
        "checkers": checkers,
        "quality_violations": quality,
        "specs": specs,
        "healthy": (quality == 0
                    and all(value == 0 for value in checkers.values())
                    and tests > 0),
    }


def _render(report: dict) -> str:
    lines = ["# System Health", ""]
    status = "HEALTHY" if report["healthy"] else "ATTENTION NEEDED"
    score = report.get("score", {})
    lines.append(f"## {status} — Health Score {score.get('score', '?')}"
                 f" (grade {score.get('grade', '?')})")
    if score:
        lines.append("  组件: " + ", ".join(
            f"{key}={value}" for key, value in score.get("breakdown", {}).items()))
    lines.extend(["", f"- 测试: {report['tests']}"])
    quality = report["quality_violations"]
    lines.append(f"- 技术债(quality): {'未知' if quality < 0 else quality}")
    for name, count in report["checkers"].items():
        lines.append(f"- 门禁 {name}: {'未知' if count < 0 else f'{count} 违规'}")
    governance = report.get("governance", {})
    lines.extend(["", "## 治理与设备织网",
                  f"- Runner 设备: {governance.get('runners', 0)} | "
                  f"交互事件: {governance.get('events', 0)}",
                  f"- 规则草案/例外/promoted: {governance.get('drafts', 0)}/"
                  f"{governance.get('exceptions', 0)}/"
                  f"{governance.get('promoted', 0)} | 真值失效: "
                  f"{governance.get('truth_invalidations', 0)}",
                  "## 规范资产",
                  f"- ui-specs: {report['specs']['ui_specs']} 篇",
                  f"- backend-specs: {report['specs']['backend_specs']} 篇",
                  f"- 规则草案: {report['specs']['rule_drafts']}",
                  f"- advance 轮数: {report['specs']['advance_rounds']}"])
    return "\n".join(lines)


def health_main(argv: list | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pi-batch.py health", description=__doc__)
    parser.add_argument("--json", action="store_true", help="machine-readable report")
    parser.add_argument("--dir", default="", help="project root (default: tool root)")
    args = parser.parse_args(argv)
    report = health_report(Path(args.dir) if args.dir else None)
    print(json.dumps(report, indent=2) if args.json else _render(report))
    return 0 if report["healthy"] else 1


if __name__ == "__main__":
    raise SystemExit(health_main())
