"""Iterative project scanner and repair planner: `pi-batch advance`.

Scans a project across every spec dimension (UI spacing/colors/styles,
frontend engineering errors/complexity, backend engineering/architecture),
groups the findings into fine-grained per-dimension batches, prioritizes
them (P0 recoverability/security > P1 visual tokens > P2 architecture),
and records the iteration state so progress can resume round after round.

Pipeline per execution round:
  1. SCAN: run all registered checkers (--json) over the target dir
  2. GROUP: classify every violation into a dimension
  3. PRIORITIZE: P0 -> P1 -> P2 batches, each with samples + suggested fix
  4. EXECUTE: only with --execute, ask one agent to repair the top batch
  5. VERIFY: run optional validators, rescan, and stop on no progress
  6. RECORD: append before/after evidence to TARGET/docs/advance/state.jsonl

Usage:
    pi-batch advance --dir ../snaplink-console          # plan (no changes)
    pi-batch advance --dir ../snaplink-console --json   # machine plan
    pi-batch advance --dir . --max-rounds 3             # round bookkeeping
    pi-batch advance --dir . --execute --max-rounds 3   # authorized repairs
    pi-batch advance --help

The default mode only produces the iteration PLAN.  Project edits require the
explicit --execute flag; execution repairs one bounded batch at a time and
stops fail-closed on agent, validator, rescan, or no-progress failures.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from . import config
from .advance_support import (
    DIMENSIONS, AdvanceStateError, CheckerError, _append_state,
    _density_bar, _known_checker_suffix, _latest_round,
    _scan_evidence, _scan_signature, _state_symlink, _total_findings,
    _validate_report, build_batches, classify_violation, format_plan,
)
from .config import log
from .models import Task
from .runner import (expand_command_placeholders, judge_verdict, run_serial,
                     run_validation)

STATE_DIR = Path("docs/advance")
STATE_FILE = STATE_DIR / "state.jsonl"


def _checker_script(name: str) -> str:
    """Locate a checker script (repo scripts/ or tools/ai-dev-gates/)."""
    candidates = [
        Path("scripts") / name,
        Path("tools/ai-dev-gates") / name,
        Path(config.TOOL_ROOT) / "scripts" / name,
    ]
    for candidate in candidates:
        if candidate.exists():
            return str(candidate)
    return str(candidates[0])


def _checker_args(script: str, target: str) -> list[str]:
    """Build the supported CLI for one registered checker."""
    args = [sys.executable, script, "--dir", target]
    name = Path(script).name
    if name == "check-ui-spec.py":
        args.append("--all")
    elif name in {"check-frontend-quality.py", "check-backend-quality.py"}:
        args.append("--strict")
    args.append("--json")
    return args


def _registered_checkers() -> list[str]:
    """Return checker names once, preserving the public registry order."""
    return list(dict.fromkeys(dim["checker"] for dim in DIMENSIONS))


def _run_checker(script: str, target: str) -> dict:
    """Run one checker and fail closed if its report cannot be trusted."""
    if not Path(script).is_file():
        raise CheckerError(f"checker not found: {script}")
    try:
        result = subprocess.run(
            _checker_args(script, target), capture_output=True, text=True,
            timeout=300)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise CheckerError(f"{script}: checker execution failed: {exc}") from exc
    if result.returncode not in (0, 1):
        detail = (result.stderr or result.stdout).strip()[:500]
        raise CheckerError(
            f"{script}: checker exited {result.returncode}: {detail}")
    try:
        document = result.stdout.lstrip()
        payload, end = json.JSONDecoder().raw_decode(document)
    except (TypeError, json.JSONDecodeError) as exc:
        raise CheckerError(f"{script}: checker emitted invalid JSON") from exc
    if not _known_checker_suffix(script, document[end:]):
        raise CheckerError(f"{script}: checker emitted unexpected trailing data")
    report = _validate_report(script, payload)
    has_findings = bool(report["violations"])
    if has_findings != (result.returncode == 1):
        raise CheckerError(
            f"{script}: checker exit code contradicts its JSON report")
    return report


def scan_project(target: str) -> dict:
    """Run all registered checkers; returns per-dimension findings."""
    reports = {
        checker: _run_checker(_checker_script(checker), target)
        for checker in _registered_checkers()
    }
    dimensions = {}
    for dim in DIMENSIONS:
        dimensions[dim["id"]] = {"label": dim["label"], "priority": dim["priority"],
                                 "fix": dim["fix"], "count": 0, "files": set(), "samples": []}
    for checker, report in reports.items():
        for violation in report.get("violations", []):
            detail = str(violation.get("detail", ""))
            dim_id = classify_violation(detail, checker)
            dim = dimensions.get(dim_id)
            if dim is None:
                continue
            dim["count"] += 1
            file_name = str(violation.get("file", ""))
            if file_name:
                dim["files"].add(file_name)
            if len(dim["samples"]) < 3:
                dim["samples"].append(detail[:90])
    for dim in dimensions.values():
        dim["files"] = sorted(dim["files"])
    checker_files = {
        checker: report["files_scanned"] for checker, report in reports.items()
    }
    return {"target": target, "files_scanned": max(checker_files.values(), default=0),
            "checker_files_scanned": checker_files, "dimensions": dimensions}


def record_round(scan: dict, round_no: int, state_file: Path | None = None) -> None:
    """Append one round's per-dimension state (resumable, append-only)."""
    path = state_file or STATE_DIR / "state.jsonl"
    entry = {
        "ts": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "round": round_no,
        "target": scan["target"],
        "total": sum(d["count"] for d in scan["dimensions"].values()),
        "dimensions": {dim_id: d["count"] for dim_id, d in scan["dimensions"].items()},
        "mode": "scan",
    }
    _append_state(path, entry)


def _state_path(target: str, override: str = "") -> Path:
    """Resolve default state inside the target; an override is cwd-relative."""
    value = Path(override).expanduser() if override else (
        Path(target).expanduser().resolve() / STATE_FILE)
    return Path(os.path.abspath(value))


def _run_recorded_rounds(target: str, first_round: int,
                         max_rounds: int, state_file: Path) -> tuple[dict, int]:
    """Record changing read-only scans; stop instead of duplicating state."""
    scan, rounds_run, previous = {}, 0, ""
    for round_no in range(first_round, first_round + max_rounds):
        scan = scan_project(target)
        signature = _scan_signature(scan)
        if signature == previous:
            break
        record_round(scan, round_no, state_file)
        rounds_run += 1
        previous = signature
        if _total_findings(scan) == 0:
            break
    return scan, rounds_run


def _advance_scan(args, state_file: Path, first_round: int) -> tuple[dict, int]:
    """Run either one planning scan or the requested recorded rescans."""
    if args.max_rounds == 0:
        return scan_project(args.dir), 0
    return _run_recorded_rounds(args.dir, first_round, args.max_rounds, state_file)


def _repair_prompt(batch: dict, round_no: int) -> str:
    payload = json.dumps(batch, ensure_ascii=False, indent=2)
    return (
        "You are executing an explicitly authorized pi-batch advance repair.\n"
        f"Round: {round_no}. Repair ONLY this highest-priority bounded batch:\n{payload}\n\n"
        "Inspect the cited files and make the smallest coherent code changes that remove "
        "these findings. Restrict edits to the listed files when that list is non-empty. "
        "Do not broaden scope, do not edit generated/vendor files, and do not claim a "
        "check passed unless you ran it. Finish with a concise change/test report."
    )


def _agent_failure(reason: str, code: int = 1) -> dict:
    return {"success": False, "status": "AGENT_FAILED", "returncode": code,
            "reason": reason[:1000], "elapsed": 0, "output": ""}


def _run_agent_batch(args, batch: dict, round_no: int) -> dict:
    errors = config.agent_preflight_errors(args.agent_bin)
    if errors:
        return _agent_failure("; ".join(errors))
    target = Path(args.dir).expanduser().resolve()
    output = target / ".pi-batch" / "advance" / f"round-{round_no}-{batch['id']}.md"
    task = Task(prompt=_repair_prompt(batch, round_no), output=str(output), cwd=str(target),
                model=args.model, provider=args.provider,
                memory={"stage": "advance", "role": "repair", "batch": batch["id"]})
    if args.timeout:
        task.timeout = args.timeout
    previous_agent, previous_stream = config.AGENT_BIN, config.STREAM_OUTPUT
    config.AGENT_BIN = args.agent_bin
    if getattr(args, "json", False):
        config.STREAM_OUTPUT = "none"
    try:
        result = run_serial([task], retries=args.retries)[0]
    except SystemExit as exc:
        return _agent_failure(f"agent runner exited {exc.code}", int(exc.code or 1))
    except Exception as exc:
        return _agent_failure(f"agent runner failed: {exc}")
    finally:
        config.AGENT_BIN, config.STREAM_OUTPUT = previous_agent, previous_stream
    return {"success": result.success,
            "status": "AGENT_PASSED" if result.success else "AGENT_FAILED",
            "returncode": result.returncode, "reason": (result.reason or "")[:1000],
            "elapsed": result.elapsed, "output": str(output)}


def _json_gate_status(output: str) -> str:
    rank = {"": 0, "pass": 1, "warn": 2, "fail": 3}
    strongest = ""
    for line in (output or "").splitlines():
        try:
            value = json.loads(line.strip()) if line.lstrip().startswith("{") else {}
        except ValueError:
            continue
        status = str(value.get("status", "")).lower() if isinstance(value, dict) else ""
        if status in rank and rank[status] > rank[strongest]:
            strongest = status
    return strongest


def _validator_failure(spec, result) -> str:
    if not result.ok:
        return f"validator exited {result.exit_code}"
    if _json_gate_status(result.stdout) == "fail":
        return "validator reported JSON status=fail"
    if spec.judge:
        verdict = judge_verdict(result.stdout)
        if verdict in ("FAIL", "REJECT"):
            return f"judge verdict={verdict}"
        if verdict is None:
            return "judge produced no reasoned VERDICT"
    return ""


def _run_validators(value: str, target: Path) -> dict:
    if not value.strip():
        return {"requested": False, "success": True,
                "status": "SKIPPED", "results": []}
    outcomes = []
    for spec in config._resolve_validator_specs(value):
        command = expand_command_placeholders(spec.cmd, str(target), str(target))
        result = run_validation(command, str(target))
        failure = _validator_failure(spec, result)
        outcomes.append({"command": command[:1000], "exit": result.exit_code,
                         "success": not failure, "reason": failure,
                         "stdout": (result.stdout or "").strip()[:1000],
                         "stderr": (result.stderr or "").strip()[:1000]})
        if failure:
            return {"requested": True, "success": False, "status": "FAILED",
                    "results": outcomes, "reason": failure}
    return {"requested": True, "success": True,
            "status": "PASSED", "results": outcomes}


def _execution_entry(round_no: int, target: str, before: dict, after: dict | None,
                     batch: dict, execution: dict) -> dict:
    return {
        "ts": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "round": round_no, "target": str(Path(target).expanduser().resolve()),
        "mode": "execute", "before": _scan_evidence(before),
        "after": _scan_evidence(after) if after else None,
        "batch": batch, "execution": execution,
        "total": _total_findings(after or before),
        "dimensions": _scan_evidence(after or before)["dimensions"],
    }


def _record_execution(state_file: Path, round_no: int, args, before: dict,
                      after: dict | None, batch: dict, execution: dict) -> None:
    _append_state(
        state_file,
        _execution_entry(round_no, args.dir, before, after, batch, execution))


def _execute_one_round(args, batch: dict, round_no: int) -> tuple[dict | None, dict, str]:
    execution = _run_agent_batch(args, batch, round_no)
    execution["options"] = {
        "agent_bin": args.agent_bin, "model": args.model,
        "provider": args.provider, "timeout": args.timeout,
        "retries": args.retries, "validate": args.validate,
    }
    validation = {"requested": bool(args.validate), "success": None,
                  "status": "NOT_RUN", "results": []}
    if execution["success"]:
        validation = _run_validators(args.validate, Path(args.dir).expanduser().resolve())
        if not validation["success"]:
            execution.update({"success": False, "status": "VALIDATION_FAILED",
                              "reason": validation.get("reason", "validation failed")})
    execution["validation"] = validation
    try:
        return scan_project(args.dir), execution, ""
    except CheckerError as exc:
        execution["agent_status"] = execution["status"]
        execution.update({"success": False, "status": "RESCAN_FAILED",
                          "reason": str(exc)[:1000]})
        return None, execution, str(exc)


def _progress_status(before: dict, after: dict, seen: set[str]) -> str:
    after_signature = _scan_signature(after)
    if after_signature == _scan_signature(before):
        return "NO_PROGRESS"
    if after_signature in seen:
        return "SCAN_CYCLE"
    if _total_findings(after) > _total_findings(before):
        return "REGRESSION"
    if _total_findings(after) == _total_findings(before):
        return "NO_PROGRESS"
    return ""


def _execute_rounds(args, state_file: Path, first_round: int) -> dict:
    before = scan_project(args.dir)
    rounds_run, seen, execution = 0, set(), {}
    if _total_findings(before) == 0:
        return {"scan": before, "rounds_run": 0, "code": 0, "execution": {}}
    limit = args.max_rounds or 1
    for round_no in range(first_round, first_round + limit):
        signature = _scan_signature(before)
        if signature in seen:
            return {"scan": before, "rounds_run": rounds_run, "code": 1,
                    "execution": {"status": "SCAN_CYCLE", "success": False}}
        seen.add(signature)
        batch = build_batches(before)[0]
        after, execution, rescan_error = _execute_one_round(args, batch, round_no)
        if rescan_error:
            _record_execution(state_file, round_no, args, before, None, batch, execution)
            raise CheckerError(rescan_error)
        rounds_run += 1
        stalled = _progress_status(before, after, seen) if execution["success"] else ""
        if stalled:
            execution.update({"success": False, "status": stalled,
                              "reason": "post-repair scan did not make unique progress"})
        _record_execution(state_file, round_no, args, before, after, batch, execution)
        if not execution["success"]:
            return {"scan": after, "rounds_run": rounds_run, "code": 1,
                    "execution": execution}
        if _total_findings(after) == 0:
            return {"scan": after, "rounds_run": rounds_run, "code": 0,
                    "execution": execution}
        before = after
    incomplete = dict(execution)
    incomplete.update({"success": False, "status": "MAX_ROUNDS_REACHED",
                       "reason": "findings remain after the configured round limit"})
    return {"scan": before, "rounds_run": rounds_run, "code": 1,
            "execution": incomplete}


def _build_parser():
    import argparse
    parser = argparse.ArgumentParser(
        prog="pi-batch.py advance",
        description="Self-driving iteration engine: scan every spec dimension, "
                    "group into batches, optionally repair one batch per round.")
    parser.add_argument("--dir", default=".", help="target project directory")
    parser.add_argument("--json", action="store_true", help="machine-readable plan")
    parser.add_argument("--execute", action="store_true",
                        help="authorize agents to edit the target project")
    parser.add_argument("--max-rounds", type=int, default=0,
                        help="maximum recorded rounds (0 = scan only; with --execute, one round)")
    parser.add_argument("--round", type=int, default=None,
                        help="first round override (default: latest state round + 1)")
    parser.add_argument("--state-file", default="",
                        help="state JSONL override (default: TARGET/docs/advance/state.jsonl)")
    parser.add_argument("--agent-bin", default=config.AGENT_BIN)
    parser.add_argument("--model", default="")
    parser.add_argument("--provider", default="")
    parser.add_argument("--timeout", type=int, default=0)
    parser.add_argument("--retries", type=int, default=0)
    parser.add_argument("--validate", default="",
                        help="comma-separated named validators or commands")
    return parser


def _parse_args(argv: list):
    parser = _build_parser()
    args = parser.parse_args(argv)
    if (args.max_rounds < 0 or args.timeout < 0 or args.retries < 0
            or (args.round is not None and args.round < 1)):
        parser.error("rounds/timeout/retries must be non-negative and --round must be >= 1")
    if not Path(args.dir).expanduser().is_dir():
        parser.error(f"--dir is not a directory: {args.dir}")
    return args


def _run_advance(args, state_file: Path) -> dict:
    first_round = args.round or (_latest_round(state_file) + 1)
    if args.execute:
        return _execute_rounds(args, state_file, first_round)
    scan, rounds_run = _advance_scan(args, state_file, first_round)
    return {"scan": scan, "rounds_run": rounds_run,
            "code": 0, "execution": {}}


def _json_outcome(args, state_file: Path, outcome: dict) -> None:
    scan, rounds_run = outcome["scan"], outcome["rounds_run"]
    print(json.dumps({"target": scan["target"],
                      "files_scanned": scan["files_scanned"],
                      "rounds_run": rounds_run,
                      "total": _total_findings(scan),
                      "batches": build_batches(scan), "execute": args.execute,
                      "state_file": str(state_file) if rounds_run else "",
                      "execution": outcome["execution"]},
                     ensure_ascii=False, indent=2))


def advance_main(argv: list) -> int:
    """Scan by default; mutate only through an explicit ``--execute``."""
    args = _parse_args(argv)
    state_file = _state_path(args.dir, args.state_file)
    try:
        outcome = _run_advance(args, state_file)
    except (AdvanceStateError, CheckerError) as exc:
        log.error("advance: %s", exc)
        if args.json:
            print(json.dumps({"target": args.dir, "error": str(exc)},
                             ensure_ascii=False))
        return 2
    scan, rounds_run = outcome["scan"], outcome["rounds_run"]
    batches = build_batches(scan)
    if rounds_run:
        log.info("advance: recorded %d round(s) in %s", rounds_run, state_file)
    if args.json:
        _json_outcome(args, state_file, outcome)
        return outcome["code"]
    print(format_plan(batches, _total_findings(scan)))
    return outcome["code"]
