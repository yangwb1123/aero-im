"""Reuse decision: is an existing artifact still reusable?

Centralizes the six reuse sites (cli._filter_reused, the four pipeline
task builders, and execute_stage's full-reuse short-circuit) behind one
contract: an artifact is reusable only when it exists, is non-empty, is
not a symlink, and (when validators are configured) still passes every
effective gate. A stale artifact is deleted so the caller regenerates it.

`--reuse-legacy` restores the old existence-only skip as an escape hatch.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from . import config
from .config import log


def _resolved_output(output_path: str, workdir: str = "") -> Path:
    path = Path(output_path)
    if not path.is_absolute():
        path = Path(workdir or os.getcwd()) / path
    # abspath normalizes cwd/.. without dereferencing the final component;
    # reuse must still be able to identify and reject an output symlink.
    return Path(os.path.abspath(path))


def _git_stdout(argv: list[str], workdir: str) -> bytes:
    """Small, read-only git query; an unavailable/non-git cwd is empty."""
    try:
        result = subprocess.run(
            argv, cwd=workdir, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, timeout=10, check=False)
    except (OSError, subprocess.SubprocessError):
        return b""
    return result.stdout if result.returncode == 0 else b""


def _changed_tracked_paths(workdir: str) -> tuple[Optional[Path], list[Path]]:
    root_raw = _git_stdout(["git", "rev-parse", "--show-toplevel"], workdir)
    if not root_raw:
        return None, []
    root = Path(os.fsdecode(root_raw.strip())).resolve()
    head = _git_stdout(["git", "rev-parse", "--verify", "HEAD"], workdir)
    command = (["git", "diff", "--name-only", "-z", "HEAD", "--"]
               if head else ["git", "ls-files", "-z"])
    names = _git_stdout(command, workdir)
    return root, [root / os.fsdecode(name) for name in names.split(b"\0") if name]


def _index_entries(root: Path) -> dict[str, str]:
    """Current index tree as path -> mode/blob, independent of commit id."""
    entries = {}
    for record in _git_stdout(["git", "ls-files", "-s", "-z"], str(root)).split(b"\0"):
        if not record or b"\t" not in record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        fields = metadata.split()
        if len(fields) == 3 and fields[2] == b"0":
            entries[os.fsdecode(raw_path)] = os.fsdecode(
                fields[0] + b" " + fields[1])
    return entries


def _dirty_paths(root: Path) -> set[str]:
    raw = _git_stdout(
        ["git", "status", "--porcelain=v1", "-z", "--untracked-files=all"],
        str(root))
    fields, paths, index = raw.split(b"\0"), set(), 0
    while index < len(fields) and fields[index]:
        entry = fields[index]
        if len(entry) >= 4:
            status, path = entry[:2], os.fsdecode(entry[3:])
            paths.add(path)
            if (b"R" in status or b"C" in status) and index + 1 < len(fields):
                index += 1
                if fields[index]:
                    paths.add(os.fsdecode(fields[index]))
        index += 1
    return paths


def _state_path_excluded(path: str, root: Path, output: Path) -> bool:
    normalized = path.replace("\\", "/")
    if (normalized == ".pi-batch" or normalized.startswith(".pi-batch/")
            or normalized.endswith(".meta.json")):
        return True
    try:
        return (root / path).resolve() == output.resolve()
    except OSError:
        return False


def _worktree_entry(root: Path, path: str) -> str:
    target = root / path
    if not target.is_file() and not target.is_symlink():
        return ""
    blob = _git_stdout(["git", "hash-object", "--", path], str(root)).strip()
    if not blob:
        return ""
    mode = "120000" if target.is_symlink() else (
        "100755" if os.access(target, os.X_OK) else "100644")
    return f"{mode} {os.fsdecode(blob)}"


def _repository_state(workdir: str, output_path: str) -> str:
    """Digest the effective worktree tree, stable across an equivalent commit."""
    root_raw = _git_stdout(["git", "rev-parse", "--show-toplevel"], workdir)
    if not root_raw:
        return ""
    root = Path(os.fsdecode(root_raw.strip())).resolve()
    excluded = _resolved_output(output_path, workdir)
    entries = _index_entries(root)
    for path in _dirty_paths(root):
        value = _worktree_entry(root, path)
        if value:
            entries[path] = value
        else:
            entries.pop(path, None)
    canonical = [(path, value) for path, value in sorted(entries.items())
                 if not _state_path_excluded(path, root, excluded)]
    return hashlib.sha256(json.dumps(
        canonical, ensure_ascii=False).encode("utf-8")).hexdigest()


def artifact_is_stale(output_path: str, workdir: str = "") -> bool:
    """True when HEAD or a tracked dirty file is newer than the artifact."""
    path = _resolved_output(output_path, workdir)
    if not path.exists():
        return False
    # An absolute output with no explicit task cwd belongs to its parent,
    # not whichever repository happened to launch pi-batch. Otherwise an
    # unrelated dirty launcher tree can invalidate a reusable /tmp artifact.
    wd = workdir or str(path.parent)
    try:
        artifact_mtime = path.stat().st_mtime
        root, changed = _changed_tracked_paths(wd)
        if root is not None:
            try:
                path.resolve().relative_to(root)
            except ValueError:
                return False
        head = _git_stdout(["git", "log", "-1", "--format=%ct"], wd)
        if head and artifact_mtime < int(head.strip()):
            return True
        candidates = [item for item in changed if item.resolve() != path]
        return any(not item.exists() or item.stat().st_mtime > artifact_mtime
                   for item in candidates)
    except (OSError, ValueError):
        return False


def reuse_decision(output_path: str, validate_cmd: Optional[str],
                   workdir: str = "", legacy: bool = False,
                   expected_fp: Optional[str] = None) -> bool:
    """Reuse only a nonempty, regular, fresh, fingerprint-matching artifact
    that passes its effective validator. ``legacy`` is existence-only."""
    path = _resolved_output(output_path, workdir)
    if legacy:
        return path.exists()
    if not path.is_file() or path.is_symlink():
        if path.is_symlink():
            log.warning("REUSE: %s is a symlink; deleting and regenerating", path)
            path.unlink(missing_ok=True)
        return False
    try:
        if path.stat().st_size == 0:
            return False
    except OSError:
        return False  # vanished between is_file and stat: fail closed
    if expected_fp is None and artifact_is_stale(str(path), workdir):
        log.warning("REUSE: %s is older than repository state; regenerating", path)
        path.unlink(missing_ok=True)
        sidecar_path(path).unlink(missing_ok=True)
        return False
    # T9 (fingerprint reuse 2.0): sidecar must match the current inputs;
    # a missing/mismatched sidecar means the inputs changed -> regenerate.
    if expected_fp is not None and not sidecar_matches(path, expected_fp):
        log.warning("REUSE: fingerprint mismatch for %s (inputs changed); regenerating", path)
        path.unlink(missing_ok=True)
        sidecar_path(path).unlink(missing_ok=True)
        return False
    wd = workdir or os.getcwd()
    # Lazy import avoids a module cycle: runner uses the worktree evidence
    # helpers below, while revalidation remains owned by runner.
    from .runner import revalidate_existing
    if revalidate_existing(path, validate_cmd, wd):
        log.info("REUSED+VALIDATED: %s", path)
        return True
    log.warning("REUSE: deleting stale artifact %s", path)
    path.unlink(missing_ok=True)
    return False


def parallel_output_conflict(mode: str, tasks: list,
                             reused_outputs: list) -> str:
    """Return a duplicated resolved output before any parallel spawn."""
    if mode != "parallel":
        return ""
    outputs = [str(task.output_path()) for task in tasks if task.output]
    outputs.extend(str(_resolved_output(path)) for path in reused_outputs)
    seen = set()
    return next((path for path in outputs if path in seen or seen.add(path)), "")


def fingerprint(prompt: str, output: str, validate_cmd: Optional[str],
                model: str = "", provider: str = "", workdir: str = "") -> str:
    """sha256 of the canonical task inputs (T9 / decision D1): prompt,
    output path, effective gate spec, and model/provider routing."""
    resolved = str(_resolved_output(output, workdir))
    state_cwd = workdir or str(Path(resolved).parent)
    repo_state = _repository_state(state_cwd, resolved)
    canon = json.dumps([prompt, resolved, validate_cmd, model, provider,
                        repo_state],
                       sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(canon.encode("utf-8")).hexdigest()


def reuse_fingerprint(task, validate_cmd: Optional[str]) -> str:
    """Fingerprint for a Task object (T9), used by the single-batch
    reuse path."""
    return fingerprint(task.prompt, str(task.output_path()), validate_cmd,
                       task.model, task.provider, task.workdir())


def sidecar_path(output: str) -> Path:
    return Path(output).with_suffix(Path(output).suffix + ".meta.json")


def write_sidecar(output: str, prompt: str, validate_cmd: Optional[str],
                  model: str = "", provider: str = "", workdir: str = "") -> None:
    """Persist the artifact's input fingerprint next to it (frozen format
    version 1; only written under --reuse-fingerprint)."""
    resolved = str(_resolved_output(output, workdir))
    meta = {
        "version": 1,
        "fingerprint": fingerprint(prompt, resolved, validate_cmd, model,
                                   provider, workdir),
        "created_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "model": model,
        "provider": provider,
    }
    try:
        sidecar_path(resolved).write_text(json.dumps(meta, ensure_ascii=False),
                                          encoding="utf-8")
    except OSError as e:
        log.warning("sidecar write failed: %s", e)


def sidecar_matches(path: Path, expected_fp: str) -> bool:
    """True when the artifact's sidecar exists, is version 1, and carries
    the expected fingerprint; anything else (missing/corrupt/older format)
    means the artifact must be regenerated."""
    try:
        meta = json.loads(sidecar_path(str(path)).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    return meta.get("version") == 1 and meta.get("fingerprint") == expected_fp


def capture_worktree_state(workdir: str) -> str:
    """Bounded, read-only failure snapshot; includes untracked path names."""
    wd = workdir or os.getcwd()
    status = _git_stdout(
        ["git", "status", "--short", "--untracked-files=all"], wd)
    if not status and not _git_stdout(["git", "rev-parse", "--git-dir"], wd):
        return ""
    digest = hashlib.sha256(status + _git_stdout(
        ["git", "diff", "--no-ext-diff", "--binary", "HEAD", "--"], wd)).hexdigest()
    display = status.decode("utf-8", errors="replace")[:32768]
    return f"digest={digest}\n{display}"


def preserve_failed_worktree(workdir: str, label: str, before: str) -> str:
    """Keep before/after evidence for task-created changes; never reset them."""
    after = capture_worktree_state(workdir)
    if not before or before == after:
        return ""
    safe = "".join(c if c.isalnum() or c in "-_" else "_" for c in label)[:80]
    target = (Path(workdir or os.getcwd()) / ".pi-batch" / "rejected" /
              f"{safe or 'task'}-{time.time_ns()}-worktree.txt")
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("BEFORE\n" + before + "\nAFTER\n" + after,
                          encoding="utf-8")
    except OSError as exc:
        log.warning("could not preserve failed worktree evidence: %s", exc)
        return ""
    log.warning("WORKTREE CHANGES PRESERVED (not cleaned): %s", target)
    return str(target)
