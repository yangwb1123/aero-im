"""Pure registries, state I/O, and presentation helpers for ``advance``."""

from __future__ import annotations

import json
import os
from pathlib import Path

from .memory_io import bounded_lines


# Dimension registry: which checker output maps to which spec dimension.
DIMENSIONS = [
    {"id": "ui_spacing", "checker": "check-ui-spec.py", "mode": "spacing",
     "priority": 1, "label": "魔法间距（非 8pt token）",
     "fix": "吸附到最近 token（scripts/check-ui-spec.py --mode spacing 逐文件核对）"},
    {"id": "ui_color", "checker": "check-ui-spec.py", "mode": "color",
     "priority": 1, "label": "硬编码颜色",
     "fix": "提取为主题 token 类（AppColors / tokens），语义映射替换"},
    {"id": "ui_style", "checker": "check-ui-spec.py", "mode": "style",
     "priority": 1, "label": "inline style",
     "fix": "改用平台样式机制（styled/token/class）"},
    {"id": "fe_errors", "checker": "check-frontend-quality.py", "mode": "error",
     "priority": 0, "label": "前端工程错误（吞异常/不安全 html/测试跳过）",
     "fix": "吞异常加日志；不安全 html 改安全渲染；去掉测试 skip"},
    {"id": "fe_n1", "checker": "check-frontend-quality.py", "mode": "n1",
     "priority": 0, "label": "循环 await（N+1）",
     "fix": "Future.wait/Promise.all 并行化或批量接口"},
    {"id": "fe_complexity", "checker": "check-frontend-quality.py", "mode": "strict",
     "priority": 2, "label": "上帝文件/复杂度/嵌套",
     "fix": "按业务能力拆组件/提 helper（克制：内聚的 API 客户端保留）"},
    {"id": "be_errors", "checker": "check-backend-quality.py", "mode": "error",
     "priority": 0, "label": "后端工程错误（吞异常/危险 DDL/依赖方向）",
     "fix": "加日志/迁移文件/修正依赖方向"},
    {"id": "be_architecture", "checker": "check-backend-quality.py", "mode": "strict",
     "priority": 2, "label": "上帝服务/单实现接口/复杂度",
     "fix": "组合优于继承、去无意义抽象（design-patterns 决策表）"},
    {"id": "knowledge", "checker": "check-knowledge-freshness.py", "mode": "doc",
     "priority": 2, "label": "知识维护（模块文档/ADR/关键字段审计）",
     "fix": "补 README/ADR（docs 决策记录）；关键金额字段加审计信号"
            "（maintenance-intelligence 六层维护第 6 层）"},
    {"id": "design_intelligence", "checker": "check-design-intelligence.py", "mode": "kpi",
     "priority": 1, "label": "设计智能（KPI 强调/空状态教学/状态双编码）",
     "fix": "按 design-intelligence 规范：大数字强调样式、空状态带动作、"
            "状态色+图标双编码"},
    {"id": "backend_experience", "checker": "check-backend-experience.py", "mode": "state",
     "priority": 2, "label": "业务闭环（状态机/审计/异步生命周期/聚合上下文）",
     "fix": "状态集中为枚举/常量集；写操作带审计；长任务异步化；搜索端点"
            "返回聚合上下文（design-intelligence 后端 01/02）"},
]

_PRIORITY_LABEL = {0: "P0", 1: "P1", 2: "P2"}
_CHECKER_STDOUT_SUMMARIES = {
    "check-frontend-quality.py": ("UI-QUALITY: OK (", " file(s) scanned)"),
    "check-backend-quality.py": ("BACKEND-QUALITY: OK (", " file(s) scanned)"),
}


class CheckerError(RuntimeError):
    """A registered checker could not produce a trustworthy JSON report."""


class AdvanceStateError(RuntimeError):
    """Advance state could not be read or appended safely."""


def _validate_report(script: str, payload: object) -> dict:
    """Validate the common checker report shape before consuming it."""
    if not isinstance(payload, dict):
        raise CheckerError(f"{script}: JSON report must be an object")
    violations = payload.get("violations")
    files_scanned = payload.get("files_scanned")
    if not isinstance(violations, list) or not all(
            isinstance(item, dict) for item in violations):
        raise CheckerError(f"{script}: JSON report has invalid violations")
    if files_scanned is None:
        payload["files_scanned"] = 0
    elif not isinstance(files_scanned, int) or files_scanned < 0:
        raise CheckerError(f"{script}: JSON report has invalid files_scanned")
    return payload


def _known_checker_suffix(script: str, suffix: str) -> bool:
    """Allow only the single success summary emitted by known JSON checkers."""
    line = suffix.strip()
    if not line:
        return True
    shape = _CHECKER_STDOUT_SUMMARIES.get(Path(script).name)
    if shape is None or "\n" in line or "\r" in line:
        return False
    prefix, ending = shape
    if not line.startswith(prefix) or not line.endswith(ending):
        return False
    return line[len(prefix):-len(ending)].isdigit()


_DETAIL_DIMENSIONS: list = [
    ("fe_errors", ["swallowed exception", "unsafe innerHTML", ".skip/.only"]),
    ("be_errors", ["without assertions", "DDL", "dependency direction",
                   "status assignment", "swallow"]),
    ("fe_n1", ["N+1", "loop body"]),
    ("fe_complexity", ["nesting depth", "god-file", "decision points",
                       "state hooks", "event handlers", "api calls"]),
    ("be_architecture", ["constructor deps", "public methods",
                         "single-implementation", "Base class"]),
    ("ui_spacing", ["spacing"]),
    ("ui_color", ["color"]),
    ("ui_style", ["style"]),
    ("knowledge", ["module_without_docs", "no_decision_records",
                   "critical_number_without_audit"]),
    ("design_intelligence", ["semantic_color_literal", "kpi_emphasis_missing",
                             "empty_state_no_action", "status_not_dual_encoded"]),
    ("backend_experience", ["status_literals_without_state_set",
                            "write_without_audit_signal",
                            "long_op_without_async_lifecycle",
                            "search_endpoint_without_context"]),
]

_CHECKER_FALLBACK = {
    "check-ui-spec.py": "ui_style",
    "check-frontend-quality.py": "fe_complexity",
    "check-backend-quality.py": "be_architecture",
    "check-knowledge-freshness.py": "knowledge",
    "check-design-intelligence.py": "design_intelligence",
    "check-backend-experience.py": "backend_experience",
}
_CHECKER_DIMENSIONS = {
    name: {dim["id"] for dim in DIMENSIONS if dim["checker"] == name}
    for name in _CHECKER_FALLBACK
}


def classify_violation(detail: str, checker: str = "") -> str:
    """Map one violation to a dimension, constrained by its source checker."""
    fallback = _CHECKER_FALLBACK.get(Path(checker).name) if checker else None
    for dimension, markers in _DETAIL_DIMENSIONS:
        if any(marker in detail for marker in markers):
            allowed = _CHECKER_DIMENSIONS.get(Path(checker).name, set())
            if not checker or dimension in allowed:
                return dimension
    return fallback or "be_architecture"


def build_batches(scan: dict) -> list:
    """P0 -> P1 -> P2 batches: one batch per dimension with findings."""
    return [
        {"priority": _PRIORITY_LABEL[value["priority"]], "id": dimension,
         "count": value["count"], "label": value["label"], "fix": value["fix"],
         "files": value["files"][:8], "samples": value["samples"]}
        for dimension, value in sorted(
            scan["dimensions"].items(), key=lambda item: (
                item[1]["priority"], -item[1]["count"]))
        if value["count"] > 0
    ]


def _state_symlink(path: Path) -> Path | None:
    return next((item for item in (path, *path.parents) if item.is_symlink()), None)


def _append_state(path: Path, entry: dict) -> None:
    if linked := _state_symlink(path):
        raise AdvanceStateError(f"refusing symlink advance state path: {linked}")
    path.parent.mkdir(parents=True, exist_ok=True)
    line = json.dumps(entry, ensure_ascii=False, sort_keys=True) + "\n"
    if len(line.encode("utf-8")) > 256 * 1024:
        raise AdvanceStateError("advance state event exceeds 262144 bytes")
    try:
        with path.open("a", encoding="utf-8") as handle:
            handle.write(line)
            handle.flush()
            os.fsync(handle.fileno())
    except OSError as exc:
        raise AdvanceStateError(f"cannot append advance state {path}: {exc}") from exc


def _latest_round(path: Path) -> int:
    """Return the highest valid round without loading the JSONL into memory."""
    if linked := _state_symlink(path):
        raise AdvanceStateError(f"refusing symlink advance state path: {linked}")
    if not path.is_file():
        return 0
    latest = 0
    try:
        for line in bounded_lines(path, 256 * 1024):
            if line is None:
                continue
            try:
                value = json.loads(line)
                number = value.get("round", 0) if isinstance(value, dict) else 0
                if isinstance(number, int) and not isinstance(number, bool):
                    latest = max(latest, number)
            except (TypeError, ValueError):
                continue
    except OSError as exc:
        raise AdvanceStateError(f"cannot read advance state {path}: {exc}") from exc
    return latest


def _density_bar(count: int, total: int, width: int = 12) -> str:
    if total <= 0:
        return ""
    filled = max(1, round(count / total * width))
    return "█" * filled + "░" * (width - filled)


def format_plan(batches: list, total: int) -> str:
    """Human-readable iteration plan (per-dimension batches, prioritized)."""
    lines = [f"## 迭代计划（共 {total} 项，按维度分批）"]
    if not batches:
        lines.append("全部维度干净——无需推进。")
        return "\n".join(lines)
    for batch in batches:
        lines.append(f"\n[{batch['priority']}] {batch['label']}（{batch['count']} 项）"
                     f" {_density_bar(batch['count'], total)}")
        lines.append(f"  建议: {batch['fix']}")
        lines.extend(f"  示例: {sample}" for sample in batch["samples"])
        if batch["files"]:
            suffix = "…" if len(batch["files"]) > 5 else ""
            lines.append("  文件: " + ", ".join(batch["files"][:5]) + suffix)
    return "\n".join(lines)


def _total_findings(scan: dict) -> int:
    return sum(dimension["count"] for dimension in scan["dimensions"].values())


def _scan_evidence(scan: dict) -> dict:
    return {
        "total": _total_findings(scan),
        "files_scanned": scan.get("files_scanned", 0),
        "dimensions": {key: value["count"]
                       for key, value in scan["dimensions"].items()},
    }


def _scan_signature(scan: dict) -> str:
    payload = {
        key: {"count": value["count"], "files": value.get("files", [])}
        for key, value in scan["dimensions"].items()
    }
    return json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
