from __future__ import annotations

"""Batch execution layer (split from cli_rounds.py).

批执行、归档、git 提交与 repo 门禁——被轮循环调用。
"""

import hashlib
import os
import sys
from pathlib import Path
from typing import Optional

from . import config
from .config import (AGENT_BIN, AGENT_DEFAULT_WORKERS, COMMIT_PREFIX_DEFAULT,
                     _resolve_validator_specs, log, yaml)
from .context_inject import inject_task_context
from .models import Pipeline, Stage, Task
from .repo_gates import capture_stage_dirty_paths, commit_stage_outputs
from .registry import mark_running, unregister
from .reuse import (reuse_decision, reuse_fingerprint, sidecar_path, write_sidecar)
from .lock import LOCK_EXIT_CODE, acquire_lock, release_lock, verify_held
from .triage import (STALL_EXIT_CODE, Circuit, StallDetector, env_signature,
                     kill_orphan, marker_info, pid_alive, residue_markers, task_key)
from .pipeline import (_append_decision_log, _archive_outputs, load_pipeline,
                       load_tasks, load_tasks_from_dir, run_pipeline)
from .runner import (_json_status, judge_verdict, kill_active_procs,
                     print_summary, run_parallel, run_serial, run_validation)
from .cmd_expand import expand_cmd
from .text_io import read_text_bounded
from .task_loading import task_from_prompt
from .cli_residue import _warn_residue_markers  # noqa: E402


def _entrypoint_override(name: str, default):
    """Honor monkeypatches through the historical pbatch.cli re-exports."""
    module = sys.modules.get("pbatch.cli")
    return getattr(module, name, default) if module is not None else default

def _add_metering_args(p) -> None:
    """T7/T8: metering events, webhook, and invocation budgets."""
    p.add_argument("--events-file", default="",
                   help="T7: append JSONL metering events (task_finish/task_fail/gate_reject/budget_cap/stall_stop)")
    p.add_argument("--webhook", default="",
                   help="T7: POST each metering event to URL (non-blocking, 5s timeout, failures logged only)")
    p.add_argument("--budget-max", type=int, default=0,
                   help="T8: cap agent invocations per run (0 = unlimited); on cap stop with exit 3")
    p.add_argument("--daily-budget", type=int, default=0,
                   help="T8: cap agent invocations per UTC day (0 = unlimited); state in --daily-state file")
    p.add_argument("--daily-state", default="",
                   help="T8: daily budget counter state file (default: .pi-batch/daily-budget.json)")
    p.add_argument("--rate-limit", type=float, default=0.0,
                   help="F line: max agent invocations per second across all workers "
                        "(0 = unlimited, default); per-provider overrides via limits.providers")
    p.add_argument("--rate-burst", type=float, default=1.0,
                   help="F line: token bucket burst capacity (default: 1)")

def _wire_metering(args) -> None:
    """T7/T8: metering + budget wiring (module-level, read by run_task)."""
    from . import metering
    metering.EVENTS_FILE = args.events_file
    metering.WEBHOOK_URL = args.webhook
    metering.BUDGET_MAX = args.budget_max
    metering.DAILY_BUDGET = args.daily_budget
    metering._DAILY_STATE = args.daily_state or str(Path(".pi-batch") / "daily-budget.json")

def _wire_limits(args) -> None:
    """F line: provider token-bucket wiring (module-level, read by run_task
    before every agent spawn). CLI flags win over pi-batch.yaml limits.*."""
    from . import ratelimit
    per_second = args.rate_limit if args.rate_limit and args.rate_limit > 0 else config.RATE_LIMIT_PER_SECOND
    burst = args.rate_burst if args.rate_burst and args.rate_burst > 0 else config.RATE_LIMIT_BURST
    ratelimit.configure(per_second, burst, config.RATE_LIMIT_PROVIDERS)

def _auto_detect_pipeline(args) -> None:
    """A YAML source with a top-level 'stages' key is a pipeline definition,
    not a task list, so `pi-batch.py pipeline.yaml --reuse` works without the
    --pipeline flag."""
    if not (args.source and not args.pipeline and yaml):
        return
    src = Path(args.source)
    if src.exists() and src.suffix in (".yaml", ".yml"):
        try:
            data = yaml.safe_load(read_text_bounded(
                src, config.INPUT_MAX_BYTES, "pipeline source")) or {}
        except Exception as exc:
            if isinstance(exc, ValueError):
                log.error("Source rejected: %s", exc)
            data = {}
        if isinstance(data, dict) and "stages" in data:
            log.info("Detected pipeline file (top-level 'stages'): switching to pipeline mode")
            args.pipeline = args.source
            args.source = ""

def _resolve_pipeline(value: str, label: str, config_key: str) -> str:
    """Locate a configured routing pipeline next to cwd, the entry script,
    or the package; fail loud when absent so --classify never silently
    degrades to a plain batch."""
    candidates = []
    if os.path.isabs(value):
        candidates.append(Path(value))
    else:
        candidates.append(Path.cwd() / value)
        script_dir = os.environ.get("PBATCH_SCRIPT_DIR", "")
        if script_dir:
            candidates.append(Path(script_dir) / value)
        candidates.append(Path(__file__).resolve().parent.parent / value)
    for candidate in candidates:
        if candidate.exists():
            return str(candidate)
    log.error("%s pipeline not found: %s (set classifier.%s in pi-batch.yaml)",
              label, value, config_key)
    sys.exit(1)

def _resolve_frontend_pipeline() -> str:
    """Frontend routing pipeline (classifier.frontend_pipeline)."""
    return _resolve_pipeline(config.CLASSIFIER_FRONTEND_PIPELINE,
                             "Frontend", "frontend_pipeline")

def _maybe_classify_route(args) -> None:
    """Compatibility wrapper for execution-time deterministic routing."""
    from .classify_route import maybe_classify_route
    maybe_classify_route(args, _load_batch_tasks)

def _setup_pipeline(args) -> Optional[Pipeline]:
    """Load the pipeline (one-time) and apply CLI overrides: git_commit,
    mode/workers (explicit flags only, scanned from argv because argparse
    cannot report defaults), and the commit-message prefix."""
    if not args.pipeline:
        return None
    pipeline = load_pipeline(args.pipeline)

    if args.git_commit and not args.no_git_commit:
        for stage in pipeline.stages:
            stage.git_commit = True
    elif args.no_git_commit:
        for stage in pipeline.stages:
            stage.git_commit = False

    if "--mode" in sys.argv:
        for stage in pipeline.stages:
            stage.mode = args.mode
    if "-w" in sys.argv or "--workers" in sys.argv:
        for stage in pipeline.stages:
            stage.workers = args.workers

    for stage in pipeline.stages:
        if stage.git_commit and not stage.commit_message:
            stage.commit_message = "%s Stage: %s" % (args.commit_prefix, stage.name)
    return pipeline

def _derive_session_name(args) -> str:
    """Reproducible session base name: explicit --session-name wins, else the
    source stem (pipeline/source/dir), else 'batch'."""
    if args.session_name:
        return args.session_name
    if args.pipeline:
        return Path(args.pipeline).stem
    if args.source:
        return Path(args.source).stem
    if args.from_dir:
        return Path(args.from_dir).name
    return "batch"

def _load_batch_tasks(args, stdin_tasks) -> list:
    """Single-batch task sources: --from-dir files, -p prompt, task file,
    or stdin (consumed once, reused by later rounds)."""
    if args.from_dir:
        try:
            tasks = load_tasks_from_dir(args.from_dir, args.suffix)
        except ValueError as exc:
            log.error("Task source rejected: %s", exc)
            sys.exit(1)
        if not tasks:
            log.error("No %s files found in %s", args.suffix, args.from_dir)
            sys.exit(1)
        return tasks
    if args.prompt:
        return [_prompt_task(args.prompt, args.output or "")]
    if args.source:
        return load_tasks(args.source)
    if stdin_tasks is not None:
        return stdin_tasks
    if getattr(args, "stdin_prompt", ""):
        return [_prompt_task(args.stdin_prompt)]
    stdin = sys.stdin.read().strip()
    if stdin:
        return [_prompt_task(stdin)]
    log.error("Provide a prompt (-p), a task file, --from-dir, or --pipeline")
    sys.exit(1)


def _prompt_task(prompt: str, output: str = "") -> Task:
    """Resolve inline/stdin prompt references before any agent starts."""
    try:
        return task_from_prompt(prompt, output=output)
    except ValueError as exc:
        log.error("Prompt rejected: %s", exc)
        raise SystemExit(1) from exc

def _apply_task_overrides(args, tasks) -> None:
    """Single-task output shortcut and global model/timeout overrides."""
    if args.output and len(tasks) == 1:
        tasks[0].output = args.output
    if args.model:
        for t in tasks:
            t.model = args.model
    if args.provider:
        for t in tasks:
            t.provider = args.provider
    if args.timeout:
        for t in tasks:
            t.timeout = args.timeout

def _run_pipeline_round(args, pipeline: Pipeline, session_name: str, cli_validate: str,
                        timeout_override: int, reuse_outputs: bool) -> tuple[bool, set]:
    """One round of pipeline mode; returns (round_failed, failed_keys).
    failed_keys drives the T5 circuit/stall governance in the 24x7 loop:
    failed task keys plus a synthetic 'stage:<name>' key for stages that
    failed without a failed task (e.g. a gate verdict FAIL). Previously the
    pipeline branch crashed with UnboundLocalError and never fed the
    circuit (round-4 finding P0-1/H4/DS-10)."""
    if args.dry_run:
        run_pipeline(pipeline, model_override=args.model, dry_run=True, reuse=reuse_outputs)
        return False, set()
    all_results, failed_stages = run_pipeline(pipeline, model_override=args.model, reuse=reuse_outputs, timeout_override=timeout_override,
                                              session_mode=args.session_mode, session_name=session_name, validate_cmd=cli_validate,
                                              decision_log=args.decision_log, archive_dir=args.archive_dir,
                                              reuse_legacy=args.reuse_legacy, fingerprint_mode=args.reuse_fingerprint,
                                              approval_file=args.approve_file,
                                              original_prompt=(args.prompt or getattr(
                                                  args, "stdin_prompt", "")),
                                              retries=getattr(args, "retries", 0),
                                              retry_delay=getattr(args, "retry_delay", 10.0),
                                              backoff=getattr(args, "retry_backoff", 2.0))
    print_summary(all_results)
    if failed_stages:
        failed_keys = {task_key(r.task) for r in all_results if not r.success}
        for s in failed_stages:
            failed_keys.add("stage:" + s)
        log.error("Failed stages: %s", ", ".join(failed_stages))
        return True, failed_keys
    # P2（使用反馈，上游 merge 移植）：阶段全部成功但个别任务失败——
    # meta_max_failed_roles 允许的角色失败属豁免（gate 已兜底），只提示不阻断。
    tolerated = [r for r in all_results if not r.success]
    if tolerated:
        log.warning("Stages passed with %d tolerated task failure(s) (within "
                    "meta_max_failed_roles): %s",
                    len(tolerated),
                    ", ".join(r.task.output for r in tolerated[:5]))
    return False, set()

def _run_batch_round(args, stdin_tasks, session_name: str, cli_validate: str, max_rounds: int, round_no: int,
                     circuit: Circuit = None) -> tuple[bool, set]:
    """One round of single-batch mode: load tasks, apply overrides and reuse
    filtering, execute, record decisions and archive on full success.
    Returns True when the round failed (caller may loop again)."""
    tasks = _entrypoint_override(
        "_load_batch_tasks", _load_batch_tasks)(args, stdin_tasks)
    _entrypoint_override(
        "_apply_task_overrides", _apply_task_overrides)(args, tasks)

    # T5: residue markers are written under each task's cwd; orphan markers
    # from a killed run must be warned about there too (round-4 finding L6).
    # GAP-3: --kill-orphans kills still-running orphaned agents.
    for d in {t.workdir() for t in tasks if t.cwd}:
        _warn_residue_markers([d], args.kill_orphans)

    if not tasks:
        log.error("No tasks to execute: a YAML source must contain a 'tasks' list (pipelines use 'stages' and are auto-detected)")
        sys.exit(1)

    git_baseline = _batch_git_baseline(args, tasks)
    tasks = _drop_isolated(tasks, circuit, round_no)
    tasks = _filter_reused(tasks, args.reuse and not args.force, cli_validate, args.reuse_legacy,
                           args.reuse_fingerprint)
    if not tasks:
        log.info("All tasks already have outputs; nothing to run")
        return False, set()

    if args.dry_run:
        _print_dry_run(tasks, args.mode)
        return False, set()

    results, round_failed = _entrypoint_override(
        "_execute_batch", _execute_batch)(tasks, args, session_name, cli_validate)
    # GAP-4 (N2): repo-scoped validators never run per artifact (they would
    # race other writers); run them once at batch end. A failure fails the
    # round so the 7x24 loop retries (and the circuit can isolate it).
    repo_keys = (_entrypoint_override(
        "_run_batch_repo_validators", _run_batch_repo_validators)(
            tasks, cli_validate)
                 if not round_failed else set())
    if repo_keys:
        round_failed = True
    return _finish_batch_round(args, tasks, results, round_failed, repo_keys,
                               circuit, cli_validate, git_baseline)

def _finish_batch_round(args, tasks: list, results: list, round_failed: bool,
                        repo_keys: set, circuit: Circuit, cli_validate: str,
                        git_baseline=None) -> tuple[bool, set]:
    """Tail of a single-batch round: decision log, fingerprint sidecars,
    failure keys, circuit feedback and the environment-signature stall
    check (T5: identical validator stderr across >=2 tasks halts the loop)."""
    if args.reuse_fingerprint:
        _write_sidecars(args, tasks, results, cli_validate)
    was_failed = round_failed
    round_failed = _finalize_batch_round(
        args, results, round_failed, git_baseline)
    if round_failed and not was_failed:
        repo_keys.add("git-commit")
    failed_keys = {task_key(r.task) for r in results if not r.success} | repo_keys
    if circuit is not None:
        for r in results:
            if not r.success:
                circuit.register_failure(task_key(r.task), r.reason or "")
    if round_failed and env_signature(results):
        log.error("ENVIRONMENT: identical validator failure across >=2 tasks; "
                  "stopping loop (exit %d)", STALL_EXIT_CODE)
        sys.exit(STALL_EXIT_CODE)
    return round_failed, failed_keys

def _drop_isolated(tasks: list, circuit: Circuit, round_no: int) -> list:
    """T5: circuit-isolated tasks are dropped from this round (a poison
    task must not burn quota forever)."""
    if circuit is None or not circuit.isolated:
        return tasks
    before = len(tasks)
    kept = [t for t in tasks if circuit.allowed(task_key(t))]
    if len(kept) < before:
        log.warning("CIRCUIT: dropped %d isolated task(s) from round %d", before - len(kept), round_no)
    return kept

def _write_sidecars(args, tasks: list, results: list, cli_validate: str) -> None:
    """T9: persist input fingerprints for successful artifacts."""
    original = {str(task.output_path()): task for task in tasks if task.output}
    for r in results:
        t = original.get(str(r.task.output_path()), r.task)
        if r.success and t.output:
            eff = t.validate if t.validate is not None else cli_validate
            write_sidecar(str(t.output_path()), t.prompt, eff, t.model,
                          t.provider, t.workdir())

def _batch_git_baseline(args, tasks: list):
    if not (args.git_commit and not args.no_git_commit):
        return None
    snapshots = [capture_stage_dirty_paths(task.workdir()) for task in tasks]
    if len({root for root, _ in snapshots}) != 1:
        log.warning("git commit refused: batch tasks span multiple repositories")
        return "", None
    return snapshots[0]


def _git_commit_single_batch(args, outputs: list, dirty_baseline=None) -> bool:
    """Commit only changes owned by this round, preserving the user index."""
    root = dirty_baseline[0] if dirty_baseline and dirty_baseline[0] else os.getcwd()
    message = "%s Single batch: %d tasks" % (args.commit_prefix, len(outputs))
    stage = Stage(name="batch", cwd=root, git_commit=True,
                  commit_message=message)
    return commit_stage_outputs(stage, outputs, dirty_baseline)

def _run_batch_repo_validators(tasks: list, cli_validate: str) -> set:
    """GAP-4 (N2): execute repo-scoped validators once per batch round
    (single-batch equivalent of the pipeline stage-end deferral). Specs are
    collected from the CLI gate plus every task's per-task validate field;
    each gate runs once against the repo root. Returns the set of synthetic
    failure keys (task_key-shaped) so the round loop retries and the
    circuit can isolate a persistently broken full-tree gate."""
    specs: list = []
    seen = set()
    for raw in [cli_validate] + [t.validate for t in tasks if t.validate]:
        for spec in _resolve_validator_specs(raw or ""):
            if spec.scope == "repo" and spec.cmd not in seen:
                seen.add(spec.cmd)
                specs.append(spec)
    if not specs:
        return set()
    wd = os.getcwd()
    failed: set = set()
    for spec in specs:
        key = _run_batch_repo_spec(spec, wd)
        if key is not None:
            failed.add(key)
    return failed

def _run_batch_repo_spec(spec, wd: str):
    """Run one repo-scoped validator once against the repo root. Returns
    the synthetic failure key (task_key-shaped) or None on success."""
    from . import metering
    cmd = expand_cmd(spec.cmd, None, wd)
    log.info("REPO VALIDATE (batch): %s", cmd)
    v = run_validation(cmd, wd)
    ok = v.ok
    status = _json_status(v.stdout) if ok else ""
    if status == "fail":
        ok = False
    if ok and spec.judge:
        verdict = judge_verdict(v.stdout)
        if verdict in ("FAIL", "REJECT") or verdict is None:
            ok = False
    if ok:
        return None
    key = hashlib.sha1(f"repo-validator:{spec.cmd}".encode("utf-8")).hexdigest()[:16]
    log.error("REPO VALIDATE FAILED%s: %s (batch round failed)",
              " (timeout)" if v.timed_out else f" (exit={v.exit_code})", cmd)
    for line in (v.stderr or "").strip().splitlines()[-10:]:
        log.error("  | %s", line)
    metering.record_event("repo_gate_fail", spec.cmd[:120],
                          v.stderr[:300] or f"exit {v.exit_code}", wd)
    return key

def _finalize_batch_round(args, results: list, round_failed: bool,
                          git_baseline=None) -> bool:
    """Decision log, git commit, and archive for a finished single-batch
    round (archive only when the round fully succeeded)."""
    if args.git_commit and not args.no_git_commit and not round_failed:
        outputs = [str(r.task.output_path()) for r in results
                   if r.success and r.task.output]
        round_failed = not _git_commit_single_batch(
            args, outputs, git_baseline)
    if args.decision_log and results:
        source_name = Path(args.source).stem if args.source else (args.from_dir or "batch")
        _append_decision_log(args.decision_log, source_name, results, not round_failed, None)

    # Rolling re-analysis produces many artifacts; archive them once a
    # round fully succeeded so the worktree stays clean.
    if not round_failed and args.archive_dir and results:
        outputs = [r.task.output for r in results if r.success and r.task.output]
        _entrypoint_override(
            "_archive_outputs", _archive_outputs)(
                outputs, args.archive_dir, "batch")
    return round_failed

def _filter_reused(tasks: list, reuse: bool, validate_cmd: str = "", reuse_legacy: bool = False,
                   fingerprint_mode: bool = False) -> list:
    """Drop tasks whose output already exists when reuse is on, so a later
    round reruns only the failures. T1: a task is reused only when its
    artifact passes reuse_decision (exists, non-empty, not a symlink, and
    every effective validator still passes); stale artifacts are deleted so
    the round regenerates them. --reuse-legacy restores existence-only."""
    if not reuse:
        return tasks
    kept = []
    for t in tasks:
        try:
            inject_task_context(t)
        except ValueError as exc:
            log.error("REUSE: context injection rejected: %s", exc)
            kept.append(t)
            continue
        if not t.output:
            kept.append(t)
            continue
        effective = t.validate if t.validate is not None else validate_cmd
        fp = reuse_fingerprint(t, effective) if fingerprint_mode else None
        if reuse_decision(str(t.output_path()), effective, t.workdir(), reuse_legacy, fp):
            continue  # reused
        kept.append(t)
    skipped = len(tasks) - len(kept)
    if skipped:
        log.info("Reuse: %d task(s) reused+validated, skipped", skipped)
    return kept

def _print_dry_run(tasks: list, mode: str) -> None:
    """Print what single-batch would execute without running it."""
    print("Tasks: %d" % len(tasks))
    print("Mode:  %s" % mode)
    print()
    for i, t in enumerate(tasks, 1):
        print("  [%d] %s..." % (i, t.prompt[:80]))
        print("      model=%s  dir=%s  output=%s" %
              (t.model or "default", t.workdir(), t.output or "(stdout)"))

def _execute_batch(tasks: list, args, session_name: str, cli_validate: str) -> tuple[list, bool]:
    """Run the single-batch tasks (serial with retries, or parallel) and
    print the summary. Returns (results, round_failed)."""
    if args.mode == "serial":
        serial = _entrypoint_override("run_serial", run_serial)
        results = serial(
            tasks, retries=args.retries, retry_delay=args.retry_delay,
            backoff=args.retry_backoff,
            min_interval=getattr(args, "min_interval", 0.0),
            session_mode=getattr(args, "session_mode", "new"),
            session_id=session_name, session_name=session_name,
            validate_cmd=cli_validate)
    else:
        parallel = _entrypoint_override("run_parallel", run_parallel)
        parallel_options = {
            "validate_cmd": cli_validate,
            "retries": args.retries,
            "retry_delay": args.retry_delay,
            "backoff": args.retry_backoff,
        }
        if hasattr(args, "min_interval"):
            parallel_options["min_interval"] = args.min_interval
        results = parallel(tasks, args.workers, **parallel_options)
    _entrypoint_override("print_summary", print_summary)(results)
    return results, any(not r.success for r in results)  # noqa: E501 行数预算内
