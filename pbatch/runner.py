"""Task execution with hard deadlines, retries, gates, and summaries."""

from __future__ import annotations

import json
import os
import random
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import replace
from concurrent.futures import ThreadPoolExecutor, as_completed
from .validation_batch import run_file_validators
from pathlib import Path
from typing import NamedTuple, Optional

from . import config
from .config import AGENT_BIN, AGENT_DEFAULT_WORKERS, _resolve_validator_specs, _resolve_validators, _session_flags, log
from .cmd_expand import expand_cmd
from .context_inject import inject_task_context
from .models import Task, TaskResult
from .runner_results import (overflow_result as _overflow_result,
                             prompt_limit_result as _prompt_limit_result)
from . import metering
from . import ratelimit
from .metering import budget_allows, budget_try_consume, record_event
from .memory import (enrich_prompt, memory_enabled, new_session_id,
                     record_task, session_id_from_flags)
from .reuse import capture_worktree_state, preserve_failed_worktree
from .session import fork_flags, has_compaction, session_size_bytes
from .triage import clear_marker, task_key, write_marker

# registry of live agent subprocesses (parallel mode: a budget cap must be
# able to kill in-flight agents so the executor shutdown is prompt)
_ACTIVE_PROCS: set = set()
_ACTIVE_LOCK = threading.Lock()
_PROCESS_POLL_WAIT = threading.Event()
_BUDGET_MESSAGE = ("BUDGET: invocation limit reached; stopping — 调整 "
                   "--max-rounds/limits 后可续跑（已保留现场）")


def _scheduled_sleep(seconds: float) -> None:
    """Wait for runner-level retry/throttle scheduling.

    Keeping this boundary separate from the stdlib ``time`` module lets
    tests replace runner waits without also changing subprocess' timeout
    polling in other threads.
    """
    time.sleep(seconds)

def _register_proc(proc: subprocess.Popen) -> None:
    with _ACTIVE_LOCK:
        _ACTIVE_PROCS.add(proc)

def _unregister_proc(proc: subprocess.Popen) -> None:
    with _ACTIVE_LOCK:
        _ACTIVE_PROCS.discard(proc)

def kill_active_procs() -> None:
    """Kill every currently-running agent process group (T8 parallel cap:
    in-flight agents must not keep the runner waiting after the cap)."""
    with _ACTIVE_LOCK:
        procs = list(_ACTIVE_PROCS)
    for proc in procs:
        _kill_group(proc)

def _read_stream(stream, prefix: str, collector: list, cap: int = 0,
                 overflow: Optional[list] = None, emit: bool = False,
                 target=None) -> None:
    """Drain and collect a child stream, optionally displaying it live.
    Collection is capped at *cap* bytes (T12e): a misbehaving agent must
    not OOM the runner before the post-hoc size check; overflow is recorded
    so the caller can reject the result. The stream is still drained (a
    blocked pipe would hang the agent), while collection and live display
    stop at the cap."""
    total = 0
    sink = target or sys.stdout
    try:
        for line in iter(lambda: stream.readline(8192), ""):
            within_cap = True
            if cap > 0:
                total += len(line.encode("utf-8", errors="replace"))
                within_cap = total <= cap
            if within_cap:
                collector.append(line)
                if emit:
                    sink.write(f"{prefix}{line}" if prefix else line)
                    sink.flush()
            elif overflow is not None:
                overflow[0] = True
    except ValueError:
        # stream closed
        pass
    finally:
        stream.close()

def agent_failure_reason(returncode: int, output: str) -> str:
    """Return a short reason when the agent result must be discarded, or ''
    when the output is a usable result. Non-zero exit, empty output, and
    provider/CLI failure signatures (quota, rate limit, auth, billing) all
    reject the result so error replies are never saved as task outputs.
    Signatures are per-agent (agent.agents.<bin>.error_patterns can replace
    the built-in table for claude/codex/... — G line)."""
    if returncode != 0:
        return f"agent exited {returncode}"
    if not output or not output.strip():
        return "agent produced no output"
    for pattern in config.agent_error_patterns():
        if pattern.search(output):
            return f"agent reported provider failure ({pattern.pattern})"
    return ""

def _budget_gate(task: Task, key: str, workdir: str, session_name: str = "") -> None:
    """Stop before spawn when the atomic parallel-safe budget is exhausted."""
    if not budget_try_consume():
        log.error(_BUDGET_MESSAGE)
        record_event("budget_cap", task.prompt, "invocation limit", workdir, session_name)
        clear_marker(key, workdir)
        raise SystemExit(3)

def session_file_path(cwd: str, session_name: str) -> Optional[str]:
    """Active pi session file for a shared/per-stage session (T10)."""
    from .session import session_file
    f = session_file(cwd, session_name)
    return str(f) if f else None

def _prompt_brief(task: Task, cmd: list) -> str:
    """One-line log summary with the prompt truncated (prompt content is
    sensitive and may be shipped by --log-file)."""
    brief_prompt = " ".join(task.prompt.split())[:80]
    if len(brief_prompt) == 80:
        return f"{cmd[0]} -p {brief_prompt}..."
    return f"{cmd[0]} -p {brief_prompt}"

def _kill_group(proc: Optional[subprocess.Popen]) -> None:
    """Kill the whole child process group, tolerating an already-dead
    process, then reap the direct child before returning."""
    if proc is None:
        return
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            proc.kill()
        except Exception:
            pass  # process already gone
    try:
        proc.wait(timeout=1)
    except (subprocess.TimeoutExpired, ChildProcessError):
    # 子进程已死/超时：清理路径（已验证有意）
        pass

def _spawn_agent(cmd: list, workdir: str, env: dict) -> subprocess.Popen:
    """Spawn the agent in its own process group so the whole child tree
    can be killed on timeout or interrupt."""
    return subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, cwd=workdir, env=env, start_new_session=True)

def _agent_not_found(task: Task) -> TaskResult:
    """The agent binary is missing from PATH: a permanent, non-retryable
    failure (never saved as an artifact)."""
    log.error(
        "'%s' not found in PATH — 请先安装 agent 或在 pi-batch.yaml 配置 "
        "agent.bin 后重试（本次任务未启动，无副作用）",
        config.AGENT_BIN,
    )
    return TaskResult(task=task, success=False, stderr=f"{config.AGENT_BIN} not found in PATH",
                      reason="agent binary not found")

def _context_rejection(task: Task) -> Optional[TaskResult]:
    try:
        if not isinstance(task.env, dict) or not all(
                isinstance(k, str) and isinstance(v, str) for k, v in task.env.items()):
            raise ValueError("task env must map strings to strings")
        inject_task_context(task)
    except ValueError as exc:
        reason = f"context injection rejected: {exc}"
        log.error("REJECTED: %s", reason)
        return TaskResult(task=task, success=False, returncode=-1,
                          stderr=str(exc), reason=reason)
    return None


def _task_invocation(task: Task, session_flags: Optional[list],
                     session_name: str):
    try:
        cmd, workdir, session_id, effective_name = _memory_invocation(
            task, session_flags, session_name)
    except (TypeError, ValueError) as exc:
        reason = f"agent adapter error: {exc}"
        log.error("REJECTED: %s", reason)
        return TaskResult(task=task, success=False, returncode=-1,
                          stderr=str(exc), reason=reason)
    if prompt_limit := _prompt_limit_result(task, cmd):
        return _attach_session(prompt_limit, session_id, effective_name, workdir)
    return cmd, workdir, session_id, effective_name

def run_task(task: Task, task_index: int = 0, total: int = 0, parallel: bool = False, session_flags: Optional[list] = None, session_name: str = "") -> TaskResult:
    if rejected := _context_rejection(task):
        return rejected
    invocation = _task_invocation(task, session_flags, session_name)
    if isinstance(invocation, TaskResult):
        return invocation
    cmd, workdir, session_id, effective_name = invocation
    start = time.monotonic()
    proc = None
    completed = False
    prefix, env, key, usage_before = _prepare_task_run(
        task, cmd, task_index, total, parallel, workdir, effective_name, session_id)
    try:
        _rate_gate(task, effective_name)
        proc = _spawn_agent(cmd, workdir, env)
        _register_proc(proc)
        write_marker(key, workdir, agent_pid=proc.pid)
        streamed = _stream_output_enabled()
        stdout_lines, stderr_lines, overflow = _stream_proc(proc, prefix, start,
                                                            task.timeout, streamed)
        completed = True
        if overflow[0]:
            result = _overflow_result(task, proc, stdout_lines, stderr_lines, start)
            return _finish_task_result(
                result, task, session_id, effective_name, workdir, usage_before)
        result = _result_from_proc(task, proc, stdout_lines, stderr_lines, time.monotonic() - start)
        result.streamed_output = streamed
        return _finish_task_result(
            result, task, session_id, effective_name, workdir, usage_before)
    except subprocess.TimeoutExpired:
        result = _timeout_result(task, proc, start)
        return _finish_task_result(
            result, task, session_id, effective_name, workdir, usage_before)
    except FileNotFoundError:
        result = _agent_not_found(task)
        return _finish_task_result(
            result, task, session_id, effective_name, workdir, usage_before)
    except Exception as e:
        elapsed = time.monotonic() - start
        log.error("ERROR  [%.1fs]  [%s]", elapsed, e)
        result = TaskResult(task=task, success=False, stderr=str(e), elapsed=elapsed, reason=str(e))
        return _finish_task_result(
            result, task, session_id, effective_name, workdir, usage_before)
    finally:
        if proc is not None and not completed:
            _kill_group(proc)
        if proc is not None:
            _unregister_proc(proc)
        clear_marker(key, workdir)

def _prepare_task_run(task: Task, cmd: list, task_index: int, total: int,
                      parallel: bool, workdir: str, session_name: str,
                      session_id: str = "") -> tuple:
    prefix = f"[task-{task_index}] " if (parallel and total > 1) else ""
    log.info(">>  %s  [model=%s]  [timeout=%ss]  [dir=%s]",
             _prompt_brief(task, cmd), task.model or "default", task.timeout, workdir)
    env = {**os.environ, **task.env}
    key = task_key(task)
    write_marker(key, workdir)
    _budget_gate(task, key, workdir, session_name)
    usage_ref = session_id or session_name
    usage = metering.session_usage(workdir, usage_ref) if metering.EVENTS_FILE else None
    return prefix, env, key, usage

def _rate_gate(task: Task, session_name: str = "") -> None:
    """F line: wait for a provider token before spawning the agent. Runs
    AFTER the budget gate so a capped run stops before waiting; blocks
    parallel workers into a queue instead of overloading the provider.
    Best-effort: an acquire timeout (or a provider-missing key) only logs."""
    if ratelimit.RATE_PER_SECOND <= 0 and not ratelimit.PROVIDER_RATES:
        return
    if not ratelimit.acquire(task.provider or "", timeout=300.0):
        log.warning("RATE: token wait timed out for provider '%s' (300s); spawning anyway",
                    task.provider or "default")

def _finish_task_result(result: TaskResult, task: Task, session_id: str,
                        session_name: str, workdir: str,
                        usage_before: Optional[dict]) -> TaskResult:
    _attach_session(result, session_id, session_name, workdir)
    _record_metering(result, task, workdir, session_id or session_name, usage_before)
    return result

def _record_metering(result: TaskResult, task: Task, workdir: str,
                     session_name: str, usage_before: Optional[dict]) -> None:
    record_event("task_finish" if result.success else "task_fail",
                 task.prompt, result.reason, workdir, session_name, usage_before)

def _memory_invocation(task: Task, session_flags: Optional[list],
                       session_name: str) -> tuple[list, str, str, str]:
    workdir = task.workdir()
    flags = session_flags
    name = session_name or task.memory.get("stage", "") or "batch"
    session_id = session_id_from_flags(flags)
    prompt = task.prompt
    if memory_enabled():
        prompt = enrich_prompt(prompt, workdir, str(task.memory.get("memory_mode", "")))
        if flags is None:
            session_id = new_session_id(name, task.prompt, workdir)
            flags = _session_flags("start", session_id, name)
    return replace(task, prompt=prompt).to_cmd(flags), workdir, session_id, name

def _attach_session(result: TaskResult, session_id: str, session_name: str,
                    workdir: str) -> TaskResult:
    result.session_id = session_id
    result.session_name = session_name
    if session_id:
        from .session import session_file
        path = session_file(workdir, session_id)
        result.raw_session = str(path) if path else ""
    return result

def _output_path_is_symlink(task: Task) -> bool:
    """T0.1: probe the UNRESOLVED output path — output_path() resolves
    symlinks away, which would hide a link planted at the output location
    and redirect the write to its target (round-4 finding H1)."""
    probe = Path(task.output)
    if not probe.is_absolute():
        probe = Path(task.workdir()) / probe
    return probe.is_symlink()

def save_result(task: Task, result: TaskResult) -> None:
    """Write a successful task result to its output file, or print to stdout.
    Failed or rejected results are never written to disk: quota or rate-limit
    replies must not become committed artifacts, so the error is logged only.
    """
    out_path = task.output_path()
    if out_path is None:
        if not result.streamed_output:
            sys.stdout.write(result.stdout)
            if result.stderr:
                sys.stderr.write(result.stderr)
        return

    if not result.success:
        log.error("NOT SAVED %s: task failed (exit=%d, %.1fs)", out_path, result.returncode, result.elapsed)
        return
    # T12e: oversized outputs are rejected, not written (runaway agents
    # must not fill the disk).
    if len(result.stdout.encode("utf-8")) > config.OUTPUT_MAX_BYTES:
        log.error("NOT SAVED %s: output exceeds %d bytes (T12e cap)", out_path, config.OUTPUT_MAX_BYTES)
        return

    out_path.parent.mkdir(parents=True, exist_ok=True)
    # T0.1 (full-SDLC pipeline): refuse symlink targets (a symlinked output
    # path could redirect the write elsewhere) and use a unique mkstemp
    # name, mirroring _save_validated's F14 hardening. Note: probe the
    # UNRESOLVED path — output_path() resolves symlinks away.
    if _output_path_is_symlink(task):
        log.error("NOT SAVED %s: output path is a symlink (refusing to follow)", out_path)
        return
    fd, tmp_name = tempfile.mkstemp(prefix=out_path.name + ".", suffix=".tmp", dir=str(out_path.parent))
    os.close(fd)
    tmp = Path(tmp_name)
    tmp.write_text(result.stdout, encoding="utf-8")
    tmp.rename(out_path)
    log.info("WROTE %s  (%d bytes)", out_path, len(result.stdout))

_RETRYABLE_REASON = re.compile(r"rate|429|quota|network|connect|unreachable|timeout", re.IGNORECASE)
# P4（使用反馈）：provider 侧模型不可用/过载——快速失败后需要更长冷却
_PROVIDER_BROKEN = re.compile(r"model is unavailable|model_unavailable|upstream request failed|server_error", re.IGNORECASE)

def _retry_wait(result: TaskResult, attempt: int, retry_delay: float, backoff: float) -> float:
    """Exponential backoff for a retry attempt; provider/network failures wait
    at least 30s so a rate-limit window can clear. F line: equal jitter
    breaks the thundering herd — parallel workers that failed together must
    not re-sync into the next retry window together (rate-limit failures
    spread within 30..60s, keeping the >=30s floor; other failures get
    +/-50% around the base)."""
    wait = retry_delay * (backoff ** (attempt - 1))
    reason = result.reason or ""
    if _PROVIDER_BROKEN.search(reason):
        # provider 模型不可用：固定 60s+ 冷却（等上游恢复），连续失败由
        # circuit/stall 治理接管；jitter 防并发惊群。
        wait = max(wait, 60.0) + random.uniform(0, 30.0)
        if attempt >= 3:
            log.warning("Provider model unavailable (attempt %d); consider "
                        "--round-delay after retries exhaust", attempt)
    elif _RETRYABLE_REASON.search(reason):
        wait = max(wait, 30.0)
        wait += random.uniform(0, min(wait, 30.0))
    else:
        wait *= random.uniform(0.5, 1.5)
    return wait

class ValidationResult(NamedTuple):
    """Outcome of one validator command execution. ok is True only when the
    command finished within the hard deadline and exited 0; exit_code is -1
    for timeout/crash (timed_out distinguishes the two)."""
    ok: bool
    exit_code: int
    stdout: str
    stderr: str
    timed_out: bool

def expand_command_placeholders(cmd: str, cwd: str, output: str = "") -> str:
    """Compatibility adapter for the centralized command expander."""
    return expand_cmd(cmd, output, cwd, config.TOOL_ROOT)


def run_validation(cmd: str, cwd: str, timeout: float = 600,
                   cap: Optional[int] = None) -> ValidationResult:
    """Run one validator command in its own process group with a hard
    deadline and bounded diagnostics. A spawn crash fails closed."""
    if not isinstance(cmd, str):
        raise TypeError("validator command must be a string")
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout < 0:
        raise ValueError("validator timeout must be non-negative")
    output_cap = config.VALIDATION_OUTPUT_MAX_BYTES if cap is None else max(1, cap)
    return _run_bounded_process(cmd, cwd, timeout, output_cap, True, "validator")

def run_argv(cmd: list[str], cwd: str, timeout: float = 30,
             cap: Optional[int] = None) -> ValidationResult:
    """Run an argv command with the same bounded process-tree contract."""
    if (not isinstance(cmd, (list, tuple)) or not cmd
            or not all(isinstance(value, str) for value in cmd)):
        raise TypeError("argv command must be a non-empty string list")
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout < 0:
        raise ValueError("process timeout must be non-negative")
    output_cap = config.COMMAND_OUTPUT_MAX_BYTES if cap is None else max(1, cap)
    return _run_bounded_process(list(cmd), cwd, timeout, output_cap, False, "process")

def _run_bounded_process(cmd, cwd: str, timeout: float, output_cap: int,
                         shell: bool, label: str) -> ValidationResult:
    try:
        proc = subprocess.Popen(cmd, shell=shell, cwd=cwd,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
    except Exception as e:
        return ValidationResult(ok=False, exit_code=-1, stdout="", stderr=str(e), timed_out=False)
    start = time.monotonic()
    try:
        out, err, out_over, err_over = _drain_process(
            proc, "", start, timeout, output_cap)
    except subprocess.TimeoutExpired:
        _kill_group(proc)
        return ValidationResult(ok=False, exit_code=-1, stdout="", stderr="", timed_out=True)
    stdout, stderr = "".join(out), "".join(err)
    if out_over[0] or err_over[0]:
        reason = f"{label} output exceeds {output_cap} bytes"
        return ValidationResult(False, -1, stdout, stderr + reason, False)
    return ValidationResult(ok=proc.returncode == 0, exit_code=proc.returncode,
                            stdout=stdout, stderr=stderr, timed_out=False)

REJECTED_EXCERPT_MAX_BYTES = 65536  # bounded diagnostics, never the deliverable

def _save_rejected_excerpt(tmp: Path, workdir: str, cmd: str, reason: str) -> Optional[str]:
    """Keep a bounded excerpt of a validator-rejected artifact under
    .pi-batch/rejected/ so humans and retry prompts can diagnose WHY the
    gate refused it (the deliverable path itself stays clean — zero
    residue at the output location, aero-id maintenance R5 finding).
    Returns the excerpt path ("" when the artifact was already gone)."""
    try:
        text = tmp.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    excerpt = text[:REJECTED_EXCERPT_MAX_BYTES]
    if len(text) > REJECTED_EXCERPT_MAX_BYTES:
        excerpt += "\n... (excerpt truncated at %d bytes)" % REJECTED_EXCERPT_MAX_BYTES
    rej_dir = Path(workdir) / ".pi-batch" / "rejected"
    try:
        rej_dir.mkdir(parents=True, exist_ok=True)
        name = tmp.name.removesuffix(".tmp") + "-" + time.strftime("%Y%m%d-%H%M%S") + ".rejected.md"
        out = rej_dir / name
        header = (
            "# Rejected artifact excerpt (diagnostics only; NOT the deliverable)\n\n"
            "- validator: %s\n- reason: %s\n- original output: %s\n\n---\n\n"
            % (cmd, reason, tmp.name)
        )
        out.write_text(header + excerpt, encoding="utf-8")
        return str(out)
    except OSError as exc:
        log.warning("could not keep rejected excerpt: %s", exc)
        return ""

def _apply_gate_commands(tmp: Path, task: Task, commands: list, result: TaskResult) -> bool:
    """Run file gate commands concurrently and fold them with AND semantics;
    a failure deletes the artifact, records the outcome on the result
    (T2 feedback fields) and returns False. T12a: a validator may reply
    with a JSON status line -- {"status":"warn"} is allowed with a
    warning, {"status":"fail"} rejects regardless of exit code. H line:
    judge validators (spec.judge) must emit VERDICT: PASS|FAIL - <reason>
    (fail closed on missing/bare verdict). Repo-scoped validators are
    deferred to the stage/batch end (N2) and skipped here."""
    workdir = task.workdir()
    validations = run_file_validators(
        commands, workdir, tmp, expand_command_placeholders,
        run_validation, log, "VALIDATE",
    )
    for spec, cmd, v in validations:
        result.validation_ok = v.ok
        result.validation_exit = v.exit_code
        result.validation_stderr = _cap_feedback(v.stderr or "")
        if not _gate_spec_passed(spec, cmd, tmp, result, v, workdir):
            return False
    result.validation_ok = True
    result.validation_exit = 0
    return True

def _reject_artifact(tmp: Path, workdir: str, cmd: str, reason: str,
                     stdout: str = "", stderr: str = "") -> None:
    """Fail-closed disposal of a gate-rejected artifact: keep a bounded
    excerpt under .pi-batch/rejected/ for diagnosis, delete the temp file
    (zero residue at the output path), log the validator output tails."""
    rej = _save_rejected_excerpt(tmp, workdir, cmd, reason)
    if rej:
        log.warning("REJECTED EXCERPT: %s", rej)
    tmp.unlink(missing_ok=True)
    log.warning("VALIDATION FAILED (%s): %s; output NOT saved", reason, cmd)
    for line in (stdout or "").strip().splitlines()[-10:]:
        log.warning("  | %s", line)
    for line in (stderr or "").strip().splitlines()[-10:]:
        log.warning("  | %s", line)

def _gate_spec_passed(spec, cmd: str, tmp: Path, result: TaskResult, v, workdir: str = "") -> bool:
    """Decide whether one gate command passed: exit 0, optional JSON status
    (T12a) and judge VERDICT protocol (fail closed on missing/bare verdict).
    Records failure feedback on the result, deletes the temp artifact and
    returns False on rejection. Rejected artifacts keep a bounded excerpt
    under .pi-batch/rejected/ for diagnosis (never at the output path)."""
    if not v.ok:
        _reject_artifact(tmp, workdir, cmd,
                         "exit=%d%s" % (v.exit_code, " (timeout)" if v.timed_out else ""),
                         v.stdout, v.stderr)
        return False
    status = _json_status(v.stdout)
    if status == "fail":
        _reject_artifact(tmp, workdir, cmd, "JSON status=fail", v.stdout, v.stderr)
        return False
    if spec.judge:
        verdict = judge_verdict(v.stdout)
        if verdict in ("FAIL", "REJECT"):
            line = next((ln.strip() for ln in (v.stdout or "").splitlines()
                         if ln.strip().upper().startswith("VERDICT:")), "")
            result.validation_stderr = _cap_feedback(f"judge {verdict}: {line}")
            _reject_artifact(tmp, workdir, cmd, "judge verdict=%s" % verdict, v.stdout, v.stderr)
            return False
        if verdict is None:
            result.validation_stderr = _cap_feedback(
                "judge produced no VERDICT: PASS|FAIL - <reason> line")
            _reject_artifact(tmp, workdir, cmd, "judge: no verdict", v.stdout, v.stderr)
            return False
        log.info("VALIDATE JUDGE PASS: %s", cmd)
    if status == "warn":
        log.warning("VALIDATE WARN: %s (output saved with warning)", cmd)
    return True

def _save_validated(task: Task, result: TaskResult, validate_cmd: str) -> bool:
    """Save a successful result through the engineering gates. The output is
    written to a temp file, every resolved validator command must exit 0
    (AND semantics), then the file is atomically renamed into place; a
    failing gate deletes the temp file and leaves no artifact. {output}
    points at the temp file so gates can inspect the generated content.
    validate_cmd is a comma-separated list of registry names or raw shell
    commands (see _resolve_validators). Returns True when saved."""
    if not result.success:
        return False
    commands = _resolve_validator_specs(validate_cmd)
    if not commands:
        save_result(task, result)
        return True
    out_path = task.output_path()
    if out_path is None:
        # no file target: nothing to validate against, print as usual
        save_result(task, result)
        return True
    # T0.1 (round-4 finding H1): the symlink probe must guard the validator
    # path too — _save_validated is used whenever any validator is
    # configured (the recommended config), and without the probe a planted
    # symlink redirects tmp.rename() to its target (arbitrary file
    # overwrite with agent-controlled bytes).
    if _output_path_is_symlink(task):
        log.error("NOT SAVED %s: output path is a symlink (refusing to follow)", out_path)
        return False
    # Local fix (self-iteration round 2, F14): unique temp name (no fixed
    # .tmp suffix a local observer could pre-create as a symlink).
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=out_path.name + ".", suffix=".tmp", dir=str(out_path.parent))
    os.close(fd)
    tmp = Path(tmp_name)
    tmp.write_text(result.stdout, encoding="utf-8")
    # Local fix (self-iteration round 2, F3): shell-escape {output}/{cwd}
    # substitutions so paths with spaces/quotes cannot inject commands.
    if not _apply_gate_commands(tmp, task, commands, result):
        return False
    tmp.rename(out_path)
    log.info("WROTE %s (validated)", out_path)
    return True

def _json_status(stdout: str) -> str:
    """T12a: extract a validator's JSON status ({"status": "pass|warn|fail"});
    empty when the output carries no JSON status line. When a validator emits
    multiple status objects, the most restrictive status wins."""
    strongest = ""
    rank = {"": 0, "pass": 1, "warn": 2, "fail": 3}
    for line in (stdout or "").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            data = json.loads(line)
        except ValueError:
            continue
        status = str(data.get("status", "")).lower()
        if status in rank and rank[status] > rank[strongest]:
            strongest = status
    return strongest

_JUDGE_VERDICT_RE = re.compile(r"^\s*\*{0,2}VERDICT\s*:\s*\*{0,2}(PASS|FAIL|REJECT)\b",
                               re.IGNORECASE | re.MULTILINE)

def judge_verdict(text: str) -> Optional[str]:
    """H line (LLM-as-judge protocol): extract a VERDICT: PASS|FAIL|REJECT
    line from a judge validator's stdout. A verdict must carry a reason on
    the same line (bare verdicts fail closed — T12b semantics); any FAIL/
    REJECT wins over an earlier PASS. None = no usable verdict."""
    first = None
    for m in _JUDGE_VERDICT_RE.finditer(text or ""):
        line_end = text.find("\n", m.end())
        if line_end == -1:
            line_end = len(text)
        rest = text[m.end():line_end].strip()
        if not rest:
            continue  # bare verdict without a reason: not a usable judgment
        verdict = m.group(1).upper()
        if verdict in ("FAIL", "REJECT"):
            return verdict
        if first is None:
            first = verdict
    return first

def _cap_feedback(text: str, max_chars: int = 4000, max_lines: int = 40) -> str:
    """Cap validator stderr for retry feedback (decision D4): 4000 chars /
    40 lines, keeping the head of the output (where CLI banners land)."""
    lines = text.splitlines()[:max_lines]
    capped = "\n".join(lines)[:max_chars]
    if len(text) > len(capped):
        capped += "\n... (truncated)"
    return capped

def _retry_task_with_feedback(task: Task, result: TaskResult) -> Task:
    """T2: build the retry task with the previous validator failure appended
    to the prompt so the agent can fix the gate problem directly (the
    feedback is capped per decision D4)."""
    if not result.validation_stderr:
        return task
    feedback = (
        f"\n\n[validator feedback from previous attempt]\n"
        f"exit={result.validation_exit}\n"
        f"{result.validation_stderr}"
    )
    return replace(task, prompt=task.prompt + feedback)

def revalidate_existing(path: Path, validate_cmd: Optional[str], workdir: str = "") -> bool:
    """Re-run the effective validators against an already-written artifact
    (the reuse path: a previously generated output is promoted only while it
    still passes every gate). Missing or empty artifacts fail closed; no
    validators apply -> trivially reusable. Returns True when the artifact
    passes all gates."""
    if not path.is_file():
        return False
    # T0.1 (full-SDLC pipeline): a symlink artifact must never be validated
    # through (the gate could execute against the victim) — fail closed.
    if path.is_symlink():
        log.warning("REVALIDATE: %s is a symlink; failing closed", path)
        return False
    try:
        empty = path.stat().st_size == 0
    except OSError:
        return False  # vanished between is_file and stat: fail closed
    if empty:
        return False
    commands = _resolve_validator_specs(validate_cmd)
    if not commands:
        return True
    wd = workdir or os.getcwd()
    validations = run_file_validators(
        commands, wd, path, expand_command_placeholders,
        run_validation, log, "REVALIDATE",
    )
    for spec, cmd, validation in validations:
        if not _revalidation_passed(spec, path, cmd, validation):
            return False
    log.info("REVALIDATED %s", path)
    return True

def _revalidate_one(spec, path: Path, wd: str) -> bool:
    """Re-run a single validator spec against an existing artifact. T12a:
    the JSON status protocol applies on the reuse path too — an exit-0
    validator reporting {"status":"fail"} must not promote the artifact
    when the fresh-save path would reject it (round-4 finding M3/P2-4).
    H line: judge validators apply the same fail-closed verdict protocol."""
    cmd = expand_cmd(spec.cmd, path, wd, config.TOOL_ROOT)
    log.info("REVALIDATE: %s", cmd)
    v = run_validation(cmd, wd)
    return _revalidation_passed(spec, path, cmd, v)


def _revalidation_passed(spec, path: Path, cmd: str, v) -> bool:
    if not v.ok:
        log.warning("REVALIDATION FAILED%s: %s; %s NOT reusable",
                    " (timeout)" if v.timed_out else f" (exit={v.exit_code})", cmd, path)
        for line in (v.stdout or "").strip().splitlines()[-10:]:
            log.warning("  | %s", line)
        for line in (v.stderr or "").strip().splitlines()[-10:]:
            log.warning("  | %s", line)
        return False
    status = _json_status(v.stdout)
    if status == "fail":
        log.warning("REVALIDATION FAILED (JSON status=fail): %s; %s NOT reusable", cmd, path)
        return False
    if status == "warn":
        log.warning("REVALIDATE WARN: %s (artifact reusable with warning)", cmd)
    if spec.judge:
        verdict = judge_verdict(v.stdout)
        if verdict in ("FAIL", "REJECT"):
            log.warning("REVALIDATION FAILED (judge verdict=%s): %s; %s NOT reusable",
                        verdict, cmd, path)
            return False
        if verdict is None:
            log.warning("REVALIDATION FAILED (judge: no verdict): %s; %s NOT reusable",
                        cmd, path)
            return False
    return True

def _result_from_proc(task: Task, proc: subprocess.Popen, stdout_lines: list, stderr_lines: list, elapsed: float) -> TaskResult:
    """Build a result; failure signatures override a nominal exit code 0."""
    stdout_text = "".join(stdout_lines)
    stderr_text = "".join(stderr_lines)
    reason = agent_failure_reason(proc.returncode, stdout_text + "\n" + stderr_text)
    success = proc.returncode == 0 and not reason
    if reason:
        # P7（使用反馈）：拒绝行内联 stderr 首行（provider 400/model
        # unavailable 一眼可见，不必翻到下一行的 FAIL 明细）。
        first_err = (stderr_text or "").strip().splitlines()
        hint = f" | {first_err[0][:200]}" if first_err and first_err[0] else ""
        log.warning("agent output REJECTED: %s%s", reason, hint)
    if success:
        log.info("OK  done  [%.1fs]  [output=%s]", elapsed, task.output or "(stdout)")
    else:
        # F2 (retro finding): failures must carry a stderr tail so root
        # causes are visible in run logs without reopening agent sessions.
        tail = "\n".join((stderr_text or "").strip().splitlines()[-3:])[:400]
        detail = f"\n  stderr: {tail}" if tail else ""
        log.warning("FAIL  [code=%d]  [%.1fs]%s", proc.returncode, elapsed, detail)
    return TaskResult(
        task=task, success=success, stdout=stdout_text, stderr=stderr_text,
        elapsed=elapsed, returncode=proc.returncode, reason=reason or "",
    )

def _timeout_result(task: Task, proc: subprocess.Popen, start: float) -> TaskResult:
    """Kill the whole process group (the direct child may have spawned
    helpers that keep pipes open) and return a timed-out result."""
    _kill_group(proc)
    elapsed = time.monotonic() - start
    log.error("TIMEOUT  [%.1fs]  [limit=%ss]", elapsed, task.timeout)
    return TaskResult(
        task=task, success=False, stderr=f"Task timed out after {task.timeout}s",
        elapsed=elapsed, returncode=-1, reason="task timed out",
    )

def _stream_output_enabled() -> bool:
    """Whether agent response bodies should be displayed while running."""
    mode = str(config.STREAM_OUTPUT).lower()
    return mode == "full" or (mode == "auto" and sys.stdout.isatty())

def _stream_proc(proc: subprocess.Popen, prefix: str, start: float,
                 timeout: int, emit: bool = False) -> tuple[list[str], list[str], list]:
    """Drain both pipes to one deadline; report stdout-cap overflow."""
    stdout_lines, stderr_lines, overflow, _ = _drain_process(
        proc, prefix, start, timeout, config.OUTPUT_MAX_BYTES, emit)
    return stdout_lines, stderr_lines, overflow

def _drain_process(proc: subprocess.Popen, prefix: str, start: float,
                   timeout: float, cap: int, emit: bool = False) -> tuple:
    """Drain both pipes with one absolute deadline and bounded collectors."""
    from threading import Thread
    stdout_lines: list[str] = []
    stderr_lines: list[str] = []
    stdout_overflow: list = [False]
    stderr_overflow: list = [False]
    args = (cap, stdout_overflow, emit, sys.stdout)
    tout = Thread(target=_read_stream, args=(proc.stdout, prefix, stdout_lines, *args), daemon=True)
    err_args = (cap, stderr_overflow, emit, sys.stderr)
    terr = Thread(target=_read_stream, args=(proc.stderr, prefix, stderr_lines, *err_args), daemon=True)
    tout.start()
    terr.start()
    deadline = start + timeout
    tout.join(timeout=max(0, deadline - time.monotonic()))
    terr.join(timeout=max(0, deadline - time.monotonic()))
    while proc.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(proc.args, timeout)
        _PROCESS_POLL_WAIT.wait(min(0.05, remaining))
    return stdout_lines, stderr_lines, stdout_overflow, stderr_overflow

def _rotation_flags(session_id: str, session_name: str, workdir: str = "") -> list:
    """T10: continue flags, or fork flags past the session size watermark
    (compaction entries are warned about). Session lookups use the TASK's
    workdir (sessions live under the task's cwd, not the process cwd —
    round-4 finding P3/L6)."""
    flags = _session_flags("continue", session_id, session_name)
    cwd = workdir or os.getcwd()
    if has_compaction(cwd, session_id):
        log.warning("SESSION: compaction entry in %s (context was shrunk by pi)", session_id)
    sfile = session_file_path(cwd, session_id)
    if sfile and session_size_bytes(cwd, session_id) > config.SESSION_MAX_BYTES:
        log.warning("SESSION ROTATION: %s exceeds %d bytes; forking",
                    sfile, config.SESSION_MAX_BYTES)
        return fork_flags(str(sfile))
    return flags

def _run_with_gate(task: Task, validate_cmd: str, index: int, total: int,
                    session_flags: Optional[list] = None, parallel: bool = False,
                    session_name: str = "") -> TaskResult:
    """Run one task and push its result through the engineering gate;
    a gate rejection marks the task failed with the exit signature."""
    worktree_before = capture_worktree_state(task.workdir())
    result = run_task(task, task_index=index, total=total, parallel=parallel, session_flags=session_flags,
                      session_name=session_name)
    if result.success:
        result.success = _save_validated(task, result, task.validate if task.validate is not None else validate_cmd)
        if not result.success:
            result.reason = (f"validation failed (exit={result.validation_exit})"
                             if result.validation_exit else "validation failed")
    if not result.success:
        evidence = preserve_failed_worktree(
            task.workdir(), task.output or f"task-{index}", worktree_before)
        if evidence:
            result.reason = f"{result.reason}; worktree evidence: {evidence}".strip("; ")
    record_task(result)
    return result

def _run_retried(task: Task, validate_cmd: str, index: int, total: int,
                 retries: int, retry_delay: float, backoff: float,
                 session_flags=None, retry_flags=None, parallel: bool = False,
                 session_name: str = "") -> TaskResult:
    """Run once plus exactly retries bounded retry attempts."""
    result = _run_with_gate(task, validate_cmd, index, total, session_flags,
                            parallel, session_name)
    for attempt in range(1, retries + 1):
        if result.success:
            break
        wait = _retry_wait(result, attempt, retry_delay, backoff)
        log.warning("RETRY %d/%d for task [%d/%d] in %.0fs (reason: %s)",
                    attempt, retries, index, total, wait,
                    result.reason or f"exit {result.returncode}")
        _scheduled_sleep(wait)
        retry_task = (_retry_task_with_feedback(task, result)
                      if result.validation_stderr else task)
        result = _run_with_gate(retry_task, validate_cmd, index, total,
                                retry_flags, parallel, session_name)
    return result


def run_serial(tasks: list[Task], retries: int = 0, retry_delay: float = 10.0,
               backoff: float = 2.0, min_interval: float = 0.0,
               session_mode: str = "new", session_id: str = "",
               session_name: str = "",
               validate_cmd: str = "") -> list[TaskResult]:
    """Run serially with validation, retries, throttling, and stable sessions."""
    results = []
    total = len(tasks)
    session_active = False
    for i, task in enumerate(tasks, 1):
        log.info("-- [%d/%d] --", i, total)
        flags = None
        if session_mode != "new":
            if not session_active:
                flags = _session_flags("start", session_id, session_name)
                session_active = True
            else:
                flags = _rotation_flags(session_id, session_name, task.workdir())
        retry_flags = (_session_flags("continue", session_id, session_name)
                       if session_mode != "new" else None)
        result = _run_retried(task, validate_cmd, i, total, retries,
                              retry_delay, backoff, flags, retry_flags,
                              session_name=session_name)
        results.append(result)
        if result.success and min_interval > 0:
            _scheduled_sleep(min_interval)
    return results


def _run_parallel_one(task: Task, index: int, total: int, validate_cmd: str,
                      retries: int, retry_delay: float,
                      backoff: float) -> Optional[TaskResult]:
    """Run one parallel task with the shared bounded retry contract."""
    try:
        return _run_retried(task, validate_cmd, index, total, retries,
                            retry_delay, backoff, parallel=True)
    except SystemExit as exc:
        if exc.code == 3:
            return None
        raise


def _collect_parallel(fut_map, total: int, min_interval: float,
                      capped: bool = False) -> tuple[list[TaskResult], bool]:
    """Drain futures, applying completion throttling and prompt cap stops."""
    results: list[TaskResult] = []
    successes = 0
    for future in as_completed(fut_map):
        result = future.result()
        if result is None:
            capped = True
            log.error(_BUDGET_MESSAGE)
        else:
            if result.success:
                if successes > 0 and min_interval > 0:
                    _scheduled_sleep(min_interval)
                successes += 1
            results.append(result)
            log.info("PROGRESS: %d/%d done", len(results), total)
        if capped:
            kill_active_procs()
    return results, capped


def run_parallel(tasks: list[Task], workers: int = 0, validate_cmd: str = "",
                 retries: int = 0, retry_delay: float = 10.0,
                 backoff: float = 2.0,
                 min_interval: float = 0.0) -> list[TaskResult]:
    """Run tasks concurrently with validation, retries, and throttling."""
    workers = workers or config.AGENT_DEFAULT_WORKERS
    total = len(tasks)
    log.info("PARALLEL x%d  (%d tasks)", workers, total)
    capped = False
    with ThreadPoolExecutor(max_workers=workers) as pool:
        fut_map = {}
        for index, task in enumerate(tasks, 1):
            if not budget_allows():
                capped = True
                log.error(_BUDGET_MESSAGE)
                break
            future = pool.submit(
                _run_parallel_one, task, index, total, validate_cmd,
                retries, retry_delay, backoff)
            fut_map[future] = task
        results, capped = _collect_parallel(
            fut_map, total, min_interval, capped)

    if capped:
        raise SystemExit(3)
    return results

def print_summary(results: list[TaskResult]) -> None:
    """Print an execution summary table."""
    total = len(results)
    succeeded = sum(1 for r in results if r.success)
    failed = total - succeeded
    total_elapsed = sum(r.elapsed for r in results)
    wall_time = max(r.elapsed for r in results) if results else 0

    print()
    print("=" * 56)
    print("  pi-batch execution report")
    print("=" * 56)
    print("  total:     %d" % total)
    print("  succeeded: %d" % succeeded)
    print("  failed:    %d" % failed)
    print("  CPU time:  %.1fs" % total_elapsed)
    print("  wall time: %.1fs" % wall_time)
    if total:
        rate = 100.0 * succeeded / total
        print("  成功率:    %.0f%%" % rate)
    if failed:
        print("  下一步:    重试用 --retries；被拒产物保留在 .pi-batch/rejected/ 可诊断")
    elif succeeded:
        print("  全部通过 ✓ — 可 --git-commit 落库或继续下一轮")
    print()
    for r in results:
        icon = "PASS" if r.success else "FAIL"
        brief = r.task.prompt[:60].replace("\n", " ")
        # T2: surface the validator failure signature in the summary line.
        sig = f"  [val exit={r.validation_exit}]" if (not r.success and r.validation_exit) else ""
        print("  %s  [%6.1fs]%s %s..." % (icon, r.elapsed, sig, brief))
    print("=" * 56)
