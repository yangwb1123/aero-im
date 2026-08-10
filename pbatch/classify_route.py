"""Execution-time classifier routing, kept separate from the main CLI."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Callable

from . import config
from .config import log


def resolve_pipeline(value: str, label: str, config_key: str) -> str:
    """Resolve a configured routing pipeline and fail loud when absent."""
    candidates = []
    if os.path.isabs(value):
        candidates.append(Path(value))
    else:
        candidates.append(Path.cwd() / value)
        script_dir = os.environ.get("PBATCH_SCRIPT_DIR", "")
        if script_dir:
            candidates.append(Path(script_dir) / value)
        candidates.append(Path(config.resolve_asset_path(value)))
    for candidate in candidates:
        if candidate.is_file():
            return str(candidate)
    log.error("%s pipeline not found: %s (set classifier.%s in pi-batch.yaml)",
              label, value, config_key)
    raise SystemExit(1)


def _log_classifications(per_task: list) -> None:
    from .classifier import FRONTEND
    for index, item in enumerate(per_task, 1):
        detail = ""
        if item.task_type == FRONTEND and (
                item.platform != "unknown" or item.profile != "unknown"):
            detail = " [%s/%s]" % (item.platform, item.profile)
        log.info("CLASSIFY task %d: %s%s (score %d, %s)",
                 index, item.task_type, detail, item.score,
                 "confident" if item.confident else "best-guess")


def maybe_classify_route(args, load_tasks: Callable) -> None:
    """Route any explicit or buffered-stdin batch before execution."""
    if not args.classify or args.pipeline:
        return
    if not (args.prompt or args.source or args.from_dir
            or getattr(args, "stdin_prompt", "")):
        log.info("--classify: no task input; running as-is")
        return
    tasks = load_tasks(args, None)
    if not tasks:
        return
    from .classifier import BACKEND, FRONTEND, classify_tasks, routing_target
    dominant, per_task = classify_tasks(tasks)
    _log_classifications(per_task)
    target = routing_target(per_task)
    if target == FRONTEND:
        pipeline = resolve_pipeline(config.CLASSIFIER_FRONTEND_PIPELINE,
                                    "Frontend", "frontend_pipeline")
    elif target == BACKEND:
        pipeline = resolve_pipeline(config.CLASSIFIER_BACKEND_PIPELINE,
                                    "Backend", "backend_pipeline")
    else:
        log.info("CLASSIFY: no confident routing majority (dominant=%s); "
                 "running as a plain batch", dominant.task_type)
        return
    log.info("CLASSIFY: confident %s majority; routing to %s", target, pipeline)
    args.pipeline = pipeline
