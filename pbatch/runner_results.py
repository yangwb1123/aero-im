"""Small failure-result constructors shared by the task runner."""

from __future__ import annotations

import time
from typing import Optional

from . import config
from .config import log
from .models import Task, TaskResult


def overflow_result(task: Task, proc, stdout_lines: list,
                    stderr_lines: list, start: float) -> TaskResult:
    """Reject output whose collected stdout exceeded the configured cap."""
    log.error("REJECTED: agent output exceeds %d bytes (T12e cap)",
              config.OUTPUT_MAX_BYTES)
    return TaskResult(
        task=task, success=False, stdout="".join(stdout_lines),
        stderr="".join(stderr_lines), elapsed=time.monotonic() - start,
        returncode=proc.returncode,
        reason=f"agent output exceeds {config.OUTPUT_MAX_BYTES} bytes (T12e cap)",
    )


def prompt_limit_result(task: Task, cmd: list) -> Optional[TaskResult]:
    """Return a rejection when the adapter prompt exceeds the argv budget."""
    prompt_index = config.agent_prompt_index(config.AGENT_BIN)
    if prompt_index >= len(cmd):
        reason = "agent adapter did not produce a prompt argument"
        return TaskResult(task=task, success=False, returncode=-1, reason=reason)
    size = len(cmd[prompt_index].encode("utf-8", errors="replace"))
    if config.PROMPT_MAX_BYTES <= 0 or size <= config.PROMPT_MAX_BYTES:
        return None
    reason = f"prompt exceeds {config.PROMPT_MAX_BYTES} bytes ({size} bytes)"
    log.error("REJECTED: %s", reason)
    return TaskResult(task=task, success=False, returncode=-1, reason=reason)
