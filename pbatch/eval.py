"""Evaluation Engineering: a regression suite for the rule system itself.

`pi-batch eval` runs the evals/*.yaml cases against the live classifier /
rule matcher / assessor and fails when a keyword or registry change broke
behavior — the "Evals 缺失，规范调整后无法知道能力是提升还是退化" gap.

Eval case schema:

    name: classifier
    cases:
      - id: c1
        input: "用 Flutter 实现 ERP 排程列表页"
        assert:
          task_type: frontend_ui     # classify_text
          platform: dart
          tier: standard             # match_rules (from scale keywords)
          has_rule: [visual-core]    # rule ids present
          lacks_rule: [async-data]   # rule ids absent
          product_level: L0_local_feature   # product_manifest
          workflow_level: L1_standard       # assessor workflow_level
          prescription_has_rule: [product-thinking]  # assess prescription rule ids present
          prescription_lacks_rule: [billing]         # assess prescription rule ids absent

Assertion keys are dispatched to the matching analyzer; list values mean
"all of these must hold". Direct ``run_suite`` callers may use the legacy
``name`` identity; persisted suites require an ``id`` so every case remains
an exact ``--filter`` target.

A case asserts via `assert:`; when `assert` is absent (or `{}`), the
case's `expect:` block becomes the assertion source and only its
*executable* keys are run (EXECUTABLE_EXPECT_KEYS — system_type, tier,
includes, excludes). Display-only expect keys (task_type, domain,
matched_tier, effective_tier) are consumed solely by _suite_domains for
coverage display and never executed. When `assert` is present, `expect`
stays inert (precedence). A case with neither `assert` nor any executable
expect key raises VacuousCaseError (fail closed — it can never be a real
regression case).

Exit codes: 0 = all pass; 1 = assertion failures (CI-friendly); 2 =
schema/vacuous-case error OR --filter matched zero cases (consistent with
load_eval_files). Exit 2 never emits a JSON payload (F-7): a filter typo
is a usage error, not a run result — the message naming the filter goes to
stderr (logging), stdout stays clean.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from . import config
from .assessor import prescription, workflow_level
from .classifier import classify_text
from .config import log, yaml
from .product import product_manifest
from .rule_matcher import match_rules
from .text_io import read_text_bounded

EVAL_DIR = Path(config.TOOL_ROOT) / "evals"

_ASSERTION_KEYS = frozenset({
    "task_type", "platform", "profile", "confident", "system_type",
    "tier", "risk", "page_types", "has_rule", "lacks_rule",
    "prescription_has_rule", "prescription_lacks_rule",
    "product_level", "workflow_level", "scale",
})
_CASE_KEYS = frozenset({"id", "name", "input", "assert", "expect"})
_EXPECT_ALIASES = {"includes": "has_rule", "excludes": "lacks_rule"}
EXECUTABLE_EXPECT_KEYS = ("system_type", "tier", "includes", "excludes")
_DISPLAY_EXPECT_KEYS = ("task_type", "domain", "matched_tier", "effective_tier")


def _case_identity(case, index: int = 0) -> str:
    if not isinstance(case, dict):
        return f"case-{index + 1}"
    return str(case.get("id") or case.get("name") or f"case-{index + 1}")


def _normalize_case(case, index: int = 0, require_id: bool = False) -> dict:
    """Normalize a case while retaining inert ``expect`` coverage data."""
    if not isinstance(case, dict):
        raise ValueError("case must be a mapping")
    unknown = sorted(set(case) - _CASE_KEYS)
    if unknown:
        raise ValueError("unknown case field(s): " + ", ".join(unknown))
    if "id" in case and "name" in case:
        raise ValueError("use either 'id' or 'name', not both")
    if require_id and not isinstance(case.get("id"), str):
        raise ValueError("case needs a non-empty 'id' (filter target)")
    case_id = str(case.get("id") or case.get("name") or "").strip()
    if not case_id:
        raise ValueError("case needs a non-empty 'id' or 'name'")
    if not isinstance(case.get("input"), str) or not case["input"].strip():
        raise ValueError(f"case '{case_id}' needs non-empty string 'input'")
    assertions = _normalize_assertions(case, case_id)
    normalized = {"id": case_id, "input": case["input"],
                  "assert": assertions}
    if isinstance(case.get("expect"), dict):
        normalized["expect"] = dict(case["expect"])
    return normalized


def _normalize_assertions(case: dict, case_id: str) -> dict:
    asserted = case.get("assert")
    expected = case.get("expect")
    if asserted is not None and not isinstance(asserted, dict):
        raise ValueError(f"case '{case_id}' 'assert' must be a mapping")
    if expected is not None and not isinstance(expected, dict):
        raise ValueError(f"case '{case_id}' 'expect' must be a mapping")
    if asserted:
        source = asserted
        allowed = _ASSERTION_KEYS | frozenset(_EXPECT_ALIASES)
    elif expected is not None:
        unknown = sorted(set(expected) - set(EXECUTABLE_EXPECT_KEYS)
                         - set(_DISPLAY_EXPECT_KEYS))
        if unknown:
            raise ValueError(
                f"case '{case_id}' has unknown expectation '{unknown[0]}'")
        source = {key: value for key, value in expected.items()
                  if key in EXECUTABLE_EXPECT_KEYS}
        allowed = frozenset(EXECUTABLE_EXPECT_KEYS)
    else:
        raise ValueError(f"case '{case_id}' needs at least one assertion")
    assertions = {}
    for raw_key, expected in source.items():
        if raw_key not in allowed:
            raise ValueError(f"case '{case_id}' has unknown assertion '{raw_key}'")
        key = _EXPECT_ALIASES.get(str(raw_key), str(raw_key))
        if key not in _ASSERTION_KEYS:
            raise ValueError(f"case '{case_id}' has unknown assertion '{raw_key}'")
        if key in assertions:
            raise ValueError(f"case '{case_id}' repeats assertion '{key}'")
        if isinstance(expected, list) and not expected:
            raise ValueError(f"case '{case_id}' assertion '{raw_key}' cannot be empty")
        assertions[key] = expected
    return assertions


def _normalize_suite(data: dict, path: Path) -> dict:
    if not isinstance(data, dict) or not isinstance(data.get("cases"), list):
        raise ValueError("needs a 'cases' list")
    unknown = sorted(set(data) - {"name", "cases"})
    if unknown:
        raise ValueError("unknown suite field(s): " + ", ".join(unknown))
    name = data.get("name", path.stem)
    if not isinstance(name, str) or not name.strip():
        raise ValueError("suite needs a non-empty string 'name'")
    cases = [_normalize_case(case, index, require_id=True)
             for index, case in enumerate(data["cases"])]
    if not cases:
        raise ValueError("needs at least one case")
    ids = [case["id"] for case in cases]
    duplicates = sorted({case_id for case_id in ids if ids.count(case_id) > 1})
    if duplicates:
        raise ValueError("duplicate case id(s): " + ", ".join(duplicates))
    return {"path": path, "name": name, "cases": cases}


class VacuousCaseError(ValueError):
    """A case has neither assert nor any executable expect key (fail closed)."""


def load_eval_files() -> list:
    """All evals/*.yaml suites; malformed files fail loudly (fail closed)."""
    suites = []
    if not yaml or not EVAL_DIR.is_dir():
        log.error("No evals/ directory — create evals/*.yaml case files")
        sys.exit(2)
    for path in sorted(EVAL_DIR.glob("*.yaml")):
        try:
            data = yaml.safe_load(read_text_bounded(
                path, config.INPUT_MAX_BYTES, "eval file")) or {}
            suites.append(_normalize_suite(data, path))
        except (OSError, ValueError, yaml.YAMLError) as exc:
            log.error("Invalid eval suite %s: %s", path, exc)
            sys.exit(2)
    if not suites:
        log.error("EVAL: executed 0 eval cases (no suites found in %s)", EVAL_DIR)
        sys.exit(2)
    return suites


def _assert_value(key: str, input_text: str) -> object:
    """Dispatch one assertion key to the live analyzer."""
    if key in ("task_type", "platform", "profile", "confident", "system_type"):
        cls = classify_text(input_text)
        return {"task_type": cls.task_type, "platform": cls.platform,
                "profile": cls.profile, "confident": cls.confident,
                "system_type": cls.system_type}[key]
    if key in ("tier", "risk", "page_types"):
        matched = match_rules(input_text)
        return {"tier": matched["tier"], "risk": matched["risk"],
                "page_types": matched["page_types"]}[key]
    if key in ("has_rule", "lacks_rule", "includes", "excludes"):
        # includes/excludes share has_rule/lacks_rule semantics: the rule-id
        # set from the live match_rules manifest.
        return {item["id"] for item in match_rules(input_text)["rules"]}
    if key in ("prescription_has_rule", "prescription_lacks_rule"):
        # Prescription-level keys evaluate the assess prescription (effective
        # tier = min(keyword tier, scale tier)), not the raw matcher manifest.
        return {item["id"] for item in prescription(input_text)["prescription"]}
    if key == "product_level":
        return product_manifest(input_text)["level"]
    if key == "workflow_level":
        return workflow_level(input_text)["level"]
    if key == "scale":
        from .assessor import scale_signal
        return scale_signal(input_text)
    raise ValueError(f"unknown eval assertion key: {key}")


def _effective_asserts(suite: dict, case: dict, case_id: str) -> dict:
    """The assertion source for a case: assert wins; otherwise the
    executable expect keys (R1 precedence). A case with neither is
    vacuous — fail closed with a typed error naming suite/case (R3)."""
    asserts = case.get("assert", {})
    if asserts:
        return asserts
    raise VacuousCaseError(
        f"{suite['name']}/{case_id}: no assert and no executable "
        f"expect keys {list(EXECUTABLE_EXPECT_KEYS)}")


def _list_assertion_failure(key: str, expected_list: list, actual: set) -> str:
    """Rule-id set semantics (has_rule/includes = all present;
    lacks_rule/excludes = none present); '' when satisfied."""
    if key in ("has_rule", "includes", "prescription_has_rule"):
        missing = [e for e in expected_list if e not in actual]
        if missing:
            return f"{key} missing {missing} (got {sorted(actual)})"
    elif key in ("lacks_rule", "excludes", "prescription_lacks_rule"):
        present = [e for e in expected_list if e in actual]
        if present:
            return f"{key} present {present} (got {sorted(actual)})"
    return ""


def run_suite(suite: dict, only_id: str = "") -> list:
    """[(case_id, ok, detail)] for one suite."""
    results = []
    for index, raw_case in enumerate(suite.get("cases", [])):
        case_id = _case_identity(raw_case, index)
        if only_id and case_id != only_id:
            continue
        try:
            case = _normalize_case(raw_case, index)
        except ValueError as exc:
            results.append((case_id, False, f"schema: {exc}"))
            continue
        assertions = _effective_asserts(suite, case, case_id)
        failures = [failure for key, expected in assertions.items()
                    if (failure := _assertion_failure(key, expected, case["input"]))]
        results.append((case_id, not failures, "; ".join(failures) if failures else ""))
    return results


def _assertion_failure(key: str, expected, input_text: str) -> str:
    try:
        actual = _assert_value(key, input_text)
    except ValueError as exc:
        return f"{key}: {exc}"
    expected_list = expected if isinstance(expected, list) else [expected]
    if key in ("has_rule", "prescription_has_rule"):
        missing = [item for item in expected_list if item not in actual]
        return f"{key} missing {missing} (got {sorted(actual)})" if missing else ""
    if key in ("lacks_rule", "prescription_lacks_rule"):
        present = [item for item in expected_list if item in actual]
        return f"{key} present {present} (got {sorted(actual)})" if present else ""
    if isinstance(actual, (list, set, tuple)):
        missing = [item for item in expected_list if item not in actual]
        return f"{key} missing {missing} (got {sorted(actual)})" if missing else ""
    if actual not in expected_list:
        return f"{key}: expected {expected_list}, got {actual!r}"
    return ""


def _count_cases(suite: dict) -> int:
    """Case count for a suite."""
    cases = suite.get("cases")
    if isinstance(cases, list):
        return len(cases)
    return 0


def _suite_domains(suite: dict) -> list:
    """Domains exercised by a normalized suite's assertion fields."""
    domains = []
    cases = suite.get("cases")
    if not isinstance(cases, list):
        return []
    for case in cases:
        if not isinstance(case, dict):
            continue
        assertions = case.get("assert", {}) or case.get("expect", {})
        for key in ("task_type", "system_type", "tier", "product_level",
                    "workflow_level", "scale", "domain", "matched_tier",
                    "effective_tier"):
            value = assertions.get(key) if isinstance(assertions, dict) else None
            values = value if isinstance(value, list) else [value]
            for item in values:
                if item and str(item) not in domains:
                    domains.append(str(item))
    return domains[:5]


def _coverage(suites: list) -> list:
    return [{"suite": suite.get("name", "?"),
             "cases": _count_cases(suite),
             "domains": _suite_domains(suite)} for suite in suites]


def _print_coverage(total: int, coverage: list) -> None:
    """Per-suite case counts + domain coverage (data-expression)."""
    print("## Eval 域覆盖")
    for item in coverage:
        print(f"- {item['suite']}: {item['cases']} 用例"
              f" (域: {', '.join(item['domains']) or 'n/a'})")
    print(f"总计 {total} 用例 / {len(coverage)} 套件")


def _evaluate_suites(suites: list, only_id: str) -> tuple[int, list]:
    """Execute normalized suites, mapping vacuous cases to usage failure."""
    total, failed = 0, []
    for suite in suites:
        try:
            results = run_suite(suite, only_id)
        except VacuousCaseError as exc:
            log.error("EVAL schema error: %s", exc)
            sys.exit(2)
        for case_id, ok, detail in results:
            total += 1
            status = "PASS" if ok else "FAIL"
            log.info("EVAL %s/%s: %s", suite["name"], case_id, status)
            if not ok:
                failed.append(f"{suite['name']}/{case_id}: {detail}")
                log.error("EVAL %s/%s FAIL: %s", suite["name"], case_id, detail)
    return total, failed


def eval_main(argv: list) -> None:
    """`pi-batch eval [--filter CASE_ID] [--json]` — run the rule-system
    regression suite."""
    import argparse
    parser = argparse.ArgumentParser(
        prog="pi-batch.py eval",
        description="Run the rule-system regression suite (evals/*.yaml).")
    parser.add_argument("--filter", default="", help="run only this case id")
    parser.add_argument(
        "--quick", action="store_true",
        help="run only core regression cases (rules + classifier domains)")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument(
        "--coverage", action="store_true",
        help="print per-suite case counts and domain coverage")
    args = parser.parse_args(argv)
    suites = load_eval_files()
    active_suites = [suite for suite in suites
                     if not args.quick or suite.get("name") in {"rules", "classifier"}]
    total, failed = _evaluate_suites(active_suites, args.filter)
    if total == 0:
        # Fail closed: zero executed cases = usage error, not "all 0 passed".
        if args.filter:
            log.error("EVAL: --filter %r matched no eval cases (exact case id "
                      "required; see evals/*.yaml)", args.filter)
        else:
            log.error("EVAL: executed 0 eval cases (no evals/*.yaml loaded?)")
        sys.exit(2)
    coverage = _coverage(active_suites) if args.coverage else []
    if args.json:
        payload = {"total": total, "failed": len(failed), "failures": failed}
        if args.coverage:
            payload["coverage"] = coverage
        print(json.dumps(payload, ensure_ascii=False, indent=2))
    elif args.coverage:
        _print_coverage(total, coverage)
    if failed:
        log.error("EVAL: %d/%d failed", len(failed), total)
        sys.exit(1)
    log.info("EVAL: all %d passed", total)
