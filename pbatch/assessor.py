"""Requirement assessment: senior-experience evaluation before rule use.

A task must not load every matching rule just because keywords hit — the
prescription is driven by an expert evaluation of the REQUIREMENT itself:

1. Completeness: which of the 8 requirement dimensions are present
   (goal / user role / main flow / data source / permission / error path /
   acceptance criteria / tech stack) — missing ones are called out with
   suggestions, because rules applied to an under-specified requirement
   are guesses.
2. Scale signal: count real requirement complexity (forms, tables, flows,
   modules) → S / M / L, which caps the effective prescription tier below
   the keyword-matched tier. A login page stays demo-tier even if the
   prompt casually mentions "企业".
3. Prescription: the minimal necessary rule set for THIS requirement —
   plus an explicit "deliberately not selected" list with reasons
   (restraint: do not pile everything on).
4. Optional two-sided check: --llm-json reconciles an independent LLM
   selection (fail closed, REQUIRED rules are veto-protected).

Usage:
    pi-batch assess "企业订单管理：新建订单表单，含支付审批流程"
    pi-batch assess "..." --json
    pi-batch assess "..." --llm-json '{"apply":[],"skip":[]}'
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

from . import config
from .classifier import classify_text
from .config import log, yaml
from .relevance import _keyword_hit
from .rule_matcher import (TIER_ORDER, domain_for, format_manifest,
                           load_registry, match_rules, reconcile)
from .text_io import read_text_bounded
from .product import format_product, product_manifest, productization_level

# Requirement completeness dimensions (bilingual presence keywords).
DIMENSIONS = [
    ("goal", "业务目标",
     ["实现", "开发", "做一个", "目标", "完成", "页面", "功能", "支持",
      "build", "implement", "create", "page", "feature"]),
    ("user_role", "用户角色",
     ["用户", "角色", "管理员", "编辑", "操作员", "业务员", "人员",
      "user", "role", "operator", "admin"]),
    ("main_flow", "主流程",
     ["流程", "步骤", "查询", "新增", "编辑", "提交", "审批", "导入", "导出",
      "flow", "workflow", "create", "approve", "search", "submit"]),
    ("data_source", "数据来源",
     ["接口", "api", "数据库", "数据", "加载", "获取", "后端", "表结构",
      "dataset", "endpoint", "backend"]),
    ("permission", "权限",
     ["权限", "登录", "认证", "授权", "permission", "auth", "login",
      "access control"]),
    ("error_path", "异常路径",
     ["失败", "异常", "错误", "超时", "重试", "冲突", "降级",
      "error", "fail", "timeout", "retry", "conflict"]),
    ("acceptance", "验收标准",
     ["验收", "测试", "通过标准", "完成标准", "acceptance", "criteria",
      "test", "checklist"]),
    ("tech_stack", "技术栈",
     ["tsx", "react", "vue", "flutter", "dart", "typescript", "ts",
      "antd", "go", "golang", "python", "java", "kotlin", "rust",
      "node", "node.js", "nodejs", "javascript", "js", "c#", ".net",
      "php", "ruby", "spring", "spring boot", "fastapi", "django",
      "flask", "gin", "nestjs", "技术栈", "框架", "组件库"]),
]

_SCALE_TIERS = {"S": "demo", "M": "standard", "L": "production"}

_SUGGESTIONS = {
    "user_role": "未提及用户角色与权限：按钮可见性/操作范围无法评估",
    "main_flow": "未描述主流程：交互链路与状态机无法设计",
    "data_source": "未提及数据来源：请求/缓存/错误映射无法设计",
    "permission": "未提及权限：审批/删除等敏感操作的可见性无法评估",
    "error_path": "未提及异常路径：失败恢复与重试策略无法设计",
    "acceptance": "未提及验收标准：完成定义缺失，无法判定交付",
    "tech_stack": "未指定技术栈：平台适配（前端框架/后端语言与框架）为假设",
}


def assess_dimensions(text: str) -> list:
    """(name, label, present) for each requirement dimension."""
    lowered = (text or "").lower()
    return [(name, label, any(_keyword_hit(lowered, term) for term in terms))
            for name, label, terms in DIMENSIONS]


def scale_signal(text: str, registry: Optional[dict] = None) -> str:
    """Requirement complexity → S / M / L. Signal keywords and weights come
    from the domain registry (frontend: forms/tables/flows; backend:
    use cases/entities/integrations/distribution)."""
    reg = registry or load_registry(domain=domain_for(text))
    signals = reg.get("signals", {})
    weights = reg.get("signal_weights", {})
    lowered = (text or "").lower()
    score = 0
    for kind, terms in signals.items():
        if any(_keyword_hit(lowered, term) for term in terms):
            score += int(weights.get(kind, 1))
    if score >= 4:
        return "L"
    if score >= 1:
        return "M"
    return "S"


_WORKFLOW_SCALE = {"S": 0, "M": 1, "L": 2}
_WORKFLOW_PRODUCT = {"L0_local_feature": 0, "L1_reusable_module": 1,
                    "L2_platform_capability": 2, "L3_product_feature": 3}


WORKFLOW_SUGGESTIONS = {
    "L0_direct": "单任务直接修改 + 快速验证门禁（无需流水线）",
    "L1_standard": "标准实现流水线：frontend-implementation 或 backend-implementation"
                   "（--classify 自动路由，plan→实现+门禁→对抗审查→gate）",
    "L2_high_risk": "examples/full-sdlc-implement.yaml：对抗审查 + 裁决门 + 验收门"
                     "（权限/支付/迁移/并发）",
    "L3_platform": "examples/full-sdlc-implement.yaml + product_thinker 审查"
                    "（产品推演 + 商业/开源要素）",
}


def workflow_level(text: str, registry: Optional[dict] = None,
                   product_level: Optional[str] = None) -> dict:
    """Risk-graded workflow selection: score = risk×2 + scale + product×3.
    Low-risk small tasks stay on a direct path; high-risk/platform work
    escalates to adversarial review + gates + product thinking."""
    reg = registry or load_registry(domain=domain_for(text))
    matched = match_rules(text, registry=reg)
    risk = 1 if matched["risk"] == "high" else 0
    scale = _WORKFLOW_SCALE.get(scale_signal(text, reg), 0)
    prod = _WORKFLOW_PRODUCT.get(product_level or productization_level(text)[0], 0)
    total = risk * 2 + scale + prod * 3
    if total <= 1:
        level = "L0_direct"
    elif total <= 3:
        level = "L1_standard"
    elif total <= 5:
        level = "L2_high_risk"
    else:
        level = "L3_platform"
    return {"level": level, "score": total,
            "risk": matched["risk"], "scale": matched["tier"],
            "suggestion": WORKFLOW_SUGGESTIONS[level]}


def _registry_rule_item(rule_id: str, rule: dict, matched: dict) -> dict:
    """Materialize a registry rule that the matcher skipped only by tier."""
    profile = matched.get("profile", "")
    template = rule.get("files_template")
    files = ([str(template).replace("{profile}", profile)]
             if template and profile else list(rule.get("files", [])))
    return {
        "id": rule_id,
        "tier": rule.get("min_tier", "standard"),
        "required": True,
        "description": rule.get("description", ""),
        "files": files,
        "evidence": {
            "scale": [], "page_types": matched.get("page_types", []),
            "risk": "high", "profile": profile,
            "prescription_override": "required_when_risk",
        },
    }


def _with_risk_rules(matched: dict, registry: dict) -> list[dict]:
    """Risk-mandated rules override scale tier in every specification domain."""
    rules = [dict(item) for item in matched.get("rules", [])]
    if matched.get("risk") != "high":
        return rules
    present = {item["id"] for item in rules}
    skipped = {item["id"]: item.get("reason", "")
               for item in matched.get("skipped", [])}
    recovered = []
    for rule_id, rule in registry.get("rules", {}).items():
        if rule_id in present or not rule.get("required_when_risk"):
            continue
        if not skipped.get(rule_id, "").startswith("tier below required"):
            continue
        recovered.append(_registry_rule_item(rule_id, rule, matched))
    insert_at = next((i for i, item in enumerate(rules)
                      if not item.get("required")), len(rules))
    return rules[:insert_at] + recovered + rules[insert_at:]


def _prescription_policy(matched: dict, registry: dict, effective: str,
                         scale: str) -> tuple[list[dict], list[dict]]:
    """Apply the scale cap and backend risk invariant to any rule manifest."""
    rank = TIER_ORDER.get(effective, 1)
    selected, not_selected = [], []
    for item in _with_risk_rules(matched, registry):
        rule = registry.get("rules", {}).get(item["id"], {})
        risk_only = bool(rule.get("required_when_risk"))
        if risk_only and matched.get("risk") != "high":
            reason = "该规则仅适用于高风险需求，本需求风险评估为 low"
        elif risk_only:
            selected.append({**item, "required": True})
            continue
        elif TIER_ORDER.get(item.get("tier", "standard"), 1) <= rank:
            selected.append(item)
            continue
        else:
            reason = (f"需求评估为 {effective} 档（规模 {scale}），"
                      f"该规则需要 {item.get('tier', 'standard')} 档")
        not_selected.append({"id": item["id"], "tier": item.get("tier", "standard"),
                             "reason": reason})
    return selected, not_selected


def _merge_not_selected(*groups: list[dict]) -> list[dict]:
    """Stable de-duplication keeps the restraint report deterministic."""
    merged = {}
    for group in groups:
        for item in group:
            merged.setdefault(item["id"], item)
    return list(merged.values())


def prescription(text: str, registry: Optional[dict] = None) -> dict:
    """The minimal necessary rule set for THIS requirement.

    Effective tier = min(keyword tier, scale tier) — a login page stays
    demo-tier even when the prompt mentions 企业; a one-off form does not
    pull architecture-budgets just because it says 生产.
    """
    reg = registry or load_registry(domain=domain_for(text))
    matched = match_rules(text, classification=classify_text(text), registry=reg)
    cls = classify_text(text)
    scale = scale_signal(text, reg)
    effective = _SCALE_TIERS.get(scale, "standard")
    selected, not_selected = _prescription_policy(
        matched, reg, effective, scale)
    dimensions = assess_dimensions(text)
    present = [name for name, _, ok in dimensions if ok]
    missing = [name for name, _, ok in dimensions if not ok]
    return {
        "requirement": " ".join((text or "").split())[:120],
        "classification": {
            "task_type": cls.task_type,
            "profile": matched["profile"], "platform": cls.platform,
            "system_type": cls.system_type,
            "system_evidence": list(cls.system_evidence),
        },
        "product": product_manifest(text),
        "workflow": workflow_level(text, reg, None),
        "scale": scale, "effective_tier": effective,
        "matched_tier": matched["tier"], "risk": matched["risk"],
        "page_types": matched["page_types"],
        "completeness": {"score": len(present), "total": len(DIMENSIONS),
                         "present": present, "missing": missing},
        "prescription": selected,
        "not_selected": not_selected,
        "suggestions": [_SUGGESTIONS[name] for name in missing
                        if name in _SUGGESTIONS],
    }


def format_assessment(assessment: dict) -> str:
    """Human-readable assessment report."""
    lines = ["## 需求评估（资深经验处方）"]
    lines.append(f"需求: {assessment['requirement']}")
    cls = assessment["classification"]
    lines.append(
        f"画像: {cls['task_type'] or 'unknown'} "
        f"[{cls['platform'] or '-'}/{cls['profile'] or 'generic'}] | "
        f"系统 {cls['system_type']} | "
        f"匹配档 {assessment['matched_tier']} → 处方档 "
        f"{assessment['effective_tier']}（规模 {assessment['scale']}）| "
        f"风险 {assessment['risk']}")
    comp = assessment["completeness"]
    missing = ", ".join(comp["missing"]) if comp["missing"] else "无"
    lines.append(f"完整性: {comp['score']}/{comp['total']} — 缺失: {missing}")
    workflow = assessment["workflow"]
    lines.append(f"工作流: {workflow['level']}（分 {workflow['score']}）— "
                 f"{workflow['suggestion']}")
    lines.append(format_product(assessment["product"]))
    selected = assessment["prescription"]
    lines.append(f"处方（最小必要规则集，共 {len(selected)} 条）:")
    for item in selected:
        marker = "必选" if item["required"] else "选用"
        lines.append(f"- [{marker}] {item['id']} ({item['tier']}): "
                     f"{item['description']}")
        for file in item["files"]:
            lines.append(f"    {file}")
    skipped = assessment["not_selected"]
    if skipped:
        lines.append("刻意未选用（克制原则）:")
        for item in skipped:
            lines.append(f"- {item['id']}: {item['reason']}")
    for suggestion in assessment["suggestions"]:
        lines.append(f"建议补充: {suggestion}")
    lines.append(_focus_advice(assessment))
    return "\n".join(lines)


def _focus_advice(assessment: dict) -> str:
    """产品思维 Attention Ranking：按需求形态给出 3 秒视觉焦点建议。

    源自 design-intelligence/01（Who→Why→What→How）与 /03
    （信息优先级：核心指标→异常→趋势→详细）。
    """
    text = (assessment.get("requirement") or "").lower()
    system_type = assessment["classification"].get("system_type", "")
    focus = "核心指标大数字 + 趋势（如达成率/金额，headline 大字号 + 环比）"
    if system_type == "state-machine":
        focus = "待办/异常置顶（如待审批 N 项，色编码 + 处理入口）"
    elif system_type == "realtime":
        focus = "实时状态色块（正常/警告/异常大色区，语义色）"
    elif system_type == "search":
        focus = "搜索框 + 即时聚合上下文（名称/数量/风险）"
    elif system_type == "optimization":
        focus = "目标指标大数字 + 约束/瓶颈摘要"
    elif "审批" in text or "工作台" in text or "dashboard" in text:
        focus = "异常优先：异常/待办置顶 + 大数字核心指标"
    return (f"视觉焦点建议（3 秒规则）: {focus} —— 重要数据占大空间、"
            "高对比；异常优先置顶；普通数据降权（design-intelligence/03）")


def _parse_llm_selection(raw: str) -> dict:
    """Validate the two-sided selection shape before policy reconciliation."""
    selection = json.loads(raw)
    if not isinstance(selection, dict):
        raise ValueError("top level must be an object")
    for key in ("apply", "skip"):
        value = selection.get(key, [])
        if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
            raise ValueError(f"{key} must be a string list")
    return selection


def _apply_llm_selection(text: str, assessment: dict, registry: dict,
                         selection: dict) -> None:
    """Reconcile LLM advice, then re-apply assessor tier/risk invariants."""
    base = match_rules(text, classification=classify_text(text), registry=registry)
    constrained = {**base, "rules": assessment["prescription"]}
    reconciled = reconcile(constrained, selection.get("apply", []),
                           selection.get("skip", []), registry)
    selected, rejected = _prescription_policy(
        reconciled, registry, assessment["effective_tier"], assessment["scale"])
    assessment["prescription"] = selected
    assessment["not_selected"] = _merge_not_selected(
        assessment["not_selected"], rejected)
    assessment["provenance"] = reconciled["provenance"]
    assessment["dropped"] = reconciled["dropped"] + [
        {"id": item["id"], "reason": "assessor-policy: " + item["reason"]}
        for item in rejected if item["id"] in reconciled.get("llm_apply", [])
    ]


def format_execution_prompt(assessment: dict) -> str:
    """Compile an assessment into a ready-to-use instruction block for the
    implementing agent — the last mile of assessment-driven execution."""
    lines = [
        "## Execution instructions (generated by pi-batch assess)",
        f"REQUIREMENT: {assessment['requirement']}",
        f"CLASSIFICATION: {assessment['classification']['task_type']} "
        f"[{assessment['classification']['platform'] or '-'}/"
        f"{assessment['classification']['profile'] or 'generic'}]",
        f"SYSTEM_TYPE: {assessment['classification']['system_type']} "
        f"(evidence: {', '.join(assessment['classification']['system_evidence']) or '-'})",
    ]
    product = assessment["product"]
    lines.append(f"PRODUCTIZATION: {product['level']} — implicit-requirement "
                 f"questions are TO-CONFIRM, never implement silently")
    for scenario in product["scenarios"]:
        lines.append(f"  questions[{scenario['scenario']}]: "
                     + " / ".join(scenario["questions"]))
    for note in product["restraint_notes"]:
        lines.append(f"  restraint: {note}")
    workflow = assessment["workflow"]
    lines.append(f"WORKFLOW: {workflow['level']} — {workflow['suggestion']}")
    lines.append("RULES (load ONLY these files):")
    for item in assessment["prescription"]:
        marker = "REQUIRED" if item["required"] else "optional"
        lines.append(f"  [{marker}] {item['id']} ({item['tier']}): "
                     f"{item['description']}")
        for file in item["files"]:
            lines.append(f"    {file}")
    lines.append("COMPLETION: finish with a completion_report (commands "
                 "executed vs not_executed, no fabricated passes).")
    return "\n".join(lines)


def _assess_text(args, parser) -> str:
    """需求文本来源：--file 或位置参数（有界读取）。"""
    if args.file:
        try:
            return read_text_bounded(Path(args.file), config.INPUT_MAX_BYTES,
                                     "assess source")
        except ValueError as exc:
            parser.error(str(exc))
    text = " ".join(args.task)
    if not text.strip():
        parser.error("Provide a requirement (positional text or --file)")
    return text


def _assess_registry(text: str, override: str, parser):
    """Load the selected domain registry, failing through argparse."""
    try:
        return (load_registry(override, domain=domain_for(text))
                if override else load_registry(domain=domain_for(text)))
    except ValueError as exc:
        parser.error(str(exc))


def print_profile_block(assessment: dict) -> None:
    """画像 + 假设记录的人类可读输出（assess 文本模式尾部）。"""
    from .profile import format_profile
    print("")
    print(format_profile(assessment["task_profile"]))
    records = assessment.get("assumptions") or []
    if not records:
        return
    print("")
    print("## 假设记录（待确认问题 → 处理决策）")
    for record in records:
        print(f"- 风险{record['assumption_risk']:.2f} "
              f"{record['statement']} → {record['verification_plan']} "
              f"[{record['status']}]")


def assess_main(argv: list) -> None:
    """`pi-batch assess "<requirement>" [--json] [--llm-json '...']`."""
    import argparse
    parser = argparse.ArgumentParser(
        prog="pi-batch.py assess",
        description="Evaluate a requirement (completeness, scale, risk) and "
                    "prescribe the minimal necessary domain rules.")
    parser.add_argument("task", nargs="*", default=[], help="requirement text")
    parser.add_argument("--file", default="",
                        help="read the requirement from FILE (markdown/txt)")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--prompt", action="store_true",
                        help="print the compiled execution-instruction block "
                             "(feed it to the implementing agent)")
    parser.add_argument("--llm-json", default="",
                        help='LLM selection JSON {"apply":[],"skip":[]} to reconcile')
    parser.add_argument("--registry", default="", help="rule registry YAML override")
    args = parser.parse_args(argv)
    text = _assess_text(args, parser)
    registry = _assess_registry(text, args.registry, parser)
    assessment = prescription(text, registry)
    if args.llm_json:
        try:
            selection = _parse_llm_selection(args.llm_json)
        except Exception as exc:
            log.error("Invalid --llm-json: %s", exc)
            sys.exit(2)
        _apply_llm_selection(text, assessment, registry, selection)
    # Multi-dimensional profile, discretion envelope and explicit
    # assumptions are additive metadata; the assessor's tier/risk policy
    # above remains authoritative for the prescribed rule set.
    # P1/P3/P4：多维画像 + 裁量包络 + 假设记录（惰性导入避免循环依赖）
    from .profile import assumption_records, task_profile
    assessment["task_profile"] = task_profile(text, registry)
    assessment["assumptions"] = assumption_records(text)
    if args.json:
        print(json.dumps(assessment, ensure_ascii=False, indent=2))
        return
    if args.prompt:
        print(format_execution_prompt(assessment))
        return
    print(format_assessment(assessment))
    print_profile_block(assessment)
