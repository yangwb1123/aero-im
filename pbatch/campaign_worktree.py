"""Git worktree lifecycle helpers for isolated Campaign pipelines."""

from __future__ import annotations

import subprocess
from pathlib import Path

from .campaign_models import CampaignSettings, Direction, safe_slug
from .campaign_state import git_worktree_evidence
from .pipeline_status import PipelineStatus


def isolated_status(returncode: int, logfile: Path) -> str:
    if returncode == 0:
        return PipelineStatus.PASSED
    try:
        tail = logfile.read_text(encoding="utf-8")[-100_000:]
    except OSError:
        tail = ""
    if "GATE REJECTED" in tail:
        return PipelineStatus.GATE_REJECTED
    if "VALIDATION FAILED" in tail:
        return PipelineStatus.VALIDATION_FAILED
    return PipelineStatus.PIPELINE_FAILED


def _git_failure(result) -> str:
    if result is None:
        return "git unavailable or timed out"
    detail = (result.stderr or result.stdout).strip()[:500]
    return detail or f"git exited {result.returncode}"


def finalize_worktree(worktree: Path, branch: str, baseline: str,
                      direction_id: str) -> tuple[str, dict]:
    """Commit every non-ignored successful-pipeline change, then prove clean."""
    if git_text(worktree, ["branch", "--show-current"]) != branch:
        evidence = git_worktree_evidence(worktree)
        return f"final campaign commit refused: worktree left branch {branch}", evidence
    added = run_git(worktree, ["add", "-A"])
    if added is None or added.returncode != 0:
        evidence = git_worktree_evidence(worktree)
        return f"final campaign git add -A failed: {_git_failure(added)}", evidence
    staged = run_git(worktree, ["diff", "--cached", "--quiet", "--exit-code"])
    if staged is None or staged.returncode not in (0, 1):
        evidence = git_worktree_evidence(worktree)
        return f"final campaign git index check failed: {_git_failure(staged)}", evidence
    if staged.returncode == 1:
        committed = run_git(
            worktree, ["commit", "-m", f"[pi-batch campaign] finalize {direction_id}"])
        if committed is None or committed.returncode != 0:
            evidence = git_worktree_evidence(worktree)
            return f"final campaign commit failed: {_git_failure(committed)}", evidence
    evidence = git_worktree_evidence(worktree)
    if not evidence["clean_commit"]:
        detail = " | ".join(evidence["dirty_evidence"][:5])
        return f"final campaign commit left worktree dirty: {detail}", evidence
    head = evidence["head_commit"]
    if not head or not git_ok(worktree, ["merge-base", "--is-ancestor", baseline, head]):
        return "final campaign HEAD is not descended from its baseline", evidence
    return "", evidence


def ensure_worktree(root: Path, settings: CampaignSettings,
                    direction: Direction) -> tuple[Path, str, str]:
    campaign_id = safe_slug(settings.name)
    base = settings.path(root, settings.worktree_root) / campaign_id
    worktree = base / direction.direction_id
    branch = f"pbatch-campaign/{campaign_id}/{direction.direction_id}"
    baseline = git_text(root, ["rev-parse", "HEAD"])
    if not baseline:
        raise RuntimeError("could not resolve repository baseline commit")
    if (worktree / ".git").exists():
        _validate_existing_worktree(root, worktree, branch)
        _sync_worktree_baseline(worktree, baseline)
        _validate_existing_worktree(root, worktree, branch)
        return worktree, branch, baseline
    worktree.parent.mkdir(parents=True, exist_ok=True)
    exists = bool(git_text(root, ["show-ref", "--verify", f"refs/heads/{branch}"]))
    args = ["worktree", "add", str(worktree), branch] if exists else [
        "worktree", "add", "-b", branch, str(worktree), "HEAD"]
    try:
        proc = subprocess.run(["git", "-C", str(root), *args], capture_output=True,
                              text=True, timeout=30, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError(f"git worktree add failed: {exc}") from exc
    if proc.returncode != 0:
        raise RuntimeError(f"git worktree add failed: {proc.stderr.strip()}")
    _validate_existing_worktree(root, worktree, branch)
    _sync_worktree_baseline(worktree, baseline)
    _validate_existing_worktree(root, worktree, branch)
    return worktree, branch, baseline


def _validate_existing_worktree(root: Path, worktree: Path, branch: str) -> None:
    actual = git_text(worktree, ["branch", "--show-current"])
    if actual != branch:
        raise RuntimeError(f"worktree branch mismatch: expected {branch}, found {actual}")
    top = git_path(worktree, ["rev-parse", "--show-toplevel"])
    root_common = git_path(root, ["rev-parse", "--git-common-dir"])
    worktree_common = git_path(worktree, ["rev-parse", "--git-common-dir"])
    if top != worktree.resolve() or not root_common or root_common != worktree_common:
        raise RuntimeError("existing worktree does not belong to the campaign repository")
    result = run_git(worktree, ["status", "--porcelain=v1", "--untracked-files=all"])
    if result is None or result.returncode != 0:
        raise RuntimeError("could not verify existing campaign worktree cleanliness")
    if result.stdout.strip():
        detail = " | ".join(result.stdout.splitlines()[:5])
        raise RuntimeError(f"existing campaign worktree is dirty: {detail}")


def _sync_worktree_baseline(worktree: Path, baseline: str) -> None:
    current = git_text(worktree, ["rev-parse", "HEAD"])
    if not current:
        raise RuntimeError("could not resolve existing worktree HEAD")
    if current == baseline or git_ok(worktree, ["merge-base", "--is-ancestor", baseline, current]):
        return
    if not git_ok(worktree, ["merge-base", "--is-ancestor", current, baseline]):
        raise RuntimeError(
            "campaign worktree diverged from repository baseline; merge or rebase it manually")
    result = run_git(worktree, ["merge", "--ff-only", baseline])
    if result is None or result.returncode != 0:
        detail = result.stderr.strip()[:300] if result is not None else "git unavailable"
        raise RuntimeError(f"could not fast-forward worktree to repository baseline: {detail}")


def git_path(root: Path, args: list[str]) -> Path | None:
    value = git_text(root, args)
    if not value:
        return None
    path = Path(value)
    return (path if path.is_absolute() else root / path).resolve()


def git_ok(root: Path, args: list[str]) -> bool:
    result = run_git(root, args)
    return result is not None and result.returncode == 0


def git_text(root: Path, args: list[str]) -> str:
    proc = run_git(root, args)
    return proc.stdout.strip() if proc is not None and proc.returncode == 0 else ""


def run_git(root: Path, args: list[str]):
    try:
        return subprocess.run(["git", "-C", str(root), *args], capture_output=True,
                              text=True, timeout=30, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return None
