"""Campaign baseline validation, liveness, and residue reporting."""

from __future__ import annotations

import threading
import time
from pathlib import Path

from . import config
from .campaign_state import _git_output
from .cmd_expand import expand_cmd
from .config import log
from .runner import run_validation


def _preflight_collect_specs(pipeline) -> list:
    """Return unique repo-scoped, deterministic validator commands."""
    from .config import _resolve_validator_specs

    specs = []
    seen = set()
    for stage in pipeline.stages:
        names = [item.strip() for item in (stage.validate_cmd or "").split(",")
                 if item.strip()]
        for name in names:
            if name in seen:
                continue
            seen.add(name)
            for spec in _resolve_validator_specs(name):
                if spec and spec.scope == "repo" and not spec.judge:
                    specs.append((name, spec.cmd))
    return specs


def _preflight_run_one(name: str, cmd: str, root: Path) -> bool:
    rendered = expand_cmd(cmd, "", str(root))
    result = run_validation(
        rendered, str(root), timeout=config.COMMAND_TIMEOUT,
        cap=config.COMMAND_OUTPUT_MAX_BYTES)
    if result.ok:
        log.info("PREFLIGHT: %s OK", name)
        return True
    tail = (result.stderr or result.stdout or "").strip().splitlines()[-3:]
    log.warning("PREFLIGHT: %s FAILED on baseline (exit=%s)%s",
                name, result.exit_code,
                " — timed out" if result.timed_out else "")
    for line in tail:
        log.warning("PREFLIGHT:   %s", line[:200])
    return False


def _preflight_validators(context) -> int:
    """Surface red repository gates before any direction pipeline starts."""
    from .campaign_pipeline import load_pipeline

    template = context.settings.path(
        context.root, context.settings.pipeline_template)
    if not template.exists():
        log.warning("PREFLIGHT: pipeline template %s missing — skipped", template)
        return 0
    try:
        pipeline = load_pipeline(str(template))
    except SystemExit:
        log.warning("PREFLIGHT: pipeline template %s unloadable — skipped", template)
        return 0
    specs = _preflight_collect_specs(pipeline)
    if not specs:
        log.info("PREFLIGHT: no repo-scoped validators in pipeline template — skipped")
        return 0
    log.info("PREFLIGHT: %d repo-scoped validator(s) against the target repo",
             len(specs))
    failures = sum(
        0 if _preflight_run_one(name, cmd, context.root) else 1
        for name, cmd in specs)
    if failures:
        _report_preflight_failures(context, failures)
    return failures


def _report_preflight_failures(context, failures: int) -> None:
    log.warning(
        "PREFLIGHT: %d repo-scoped validator(s) red at HEAD — implement "
        "stages will fail closed on this baseline regardless of agent "
        "quality; clean the baseline (or add exemptions) before running "
        "pipelines, or accept the cost.", failures)
    if getattr(context.args, "preflight_strict", False):
        log.error("PREFLIGHT: --preflight-strict — aborting before pipelines")
        raise SystemExit(4)


def _report_working_tree_residue(context) -> None:
    """Report bounded Git residue left by direction pipelines."""
    result = _git_output(context.root, ["status", "--short"])
    lines = [line for line in (result or "").splitlines() if line.strip()]
    if not lines:
        log.info("WORKTREE: clean (no residue from pipeline stages)")
        return
    modified = sum(1 for line in lines
                   if line.startswith(" M") or line.startswith("M"))
    untracked = sum(1 for line in lines if line.startswith("??"))
    staged = sum(1 for line in lines
                 if line.startswith("A ") or line.startswith("M ")
                 or line.startswith("D "))
    log.warning(
        "WORKTREE: %d uncommitted file(s) left by pipeline stages "
        "(%d modified, %d untracked, %d staged) — triage before merging; "
        "gated-out pipelines still leave verified code in the tree",
        len(lines), modified, untracked, staged)
    for line in lines[:15]:
        log.warning("WORKTREE:   %s", line)
    if len(lines) > 15:
        log.warning("WORKTREE:   ... and %d more", len(lines) - 15)


_HEARTBEAT_SECONDS = 300


def _start_heartbeat() -> None:
    """Start a low-noise daemon heartbeat for long-running campaigns."""
    def beat() -> None:
        while True:
            time.sleep(_HEARTBEAT_SECONDS)
            log.info("CAMPAIGN HEARTBEAT: alive after %s", time.strftime("%H:%M:%S"))

    threading.Thread(
        target=beat, name="campaign-heartbeat", daemon=True).start()
