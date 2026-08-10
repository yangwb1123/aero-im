"""Repository-level campaign configuration and compatibility facade."""

from __future__ import annotations

import argparse
import math
import time
from dataclasses import dataclass
from pathlib import Path

from . import config
from .campaign_models import CampaignSettings
from .campaign_paths import (campaign_config_path as _campaign_config_path,
                             validate_paths as _validate_paths)
from .campaign_state import StateStore, git_snapshot
from .config import log, yaml
from .text_io import read_text_bounded


@dataclass
class CampaignContext:
    root: Path
    settings: CampaignSettings
    args: argparse.Namespace
    store: StateStore
    snapshot: dict


def build_campaign_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pi-batch.py campaign",
        description="Discover repository modules and run evidence-backed SDLC campaigns")
    parser.add_argument("--config", default="examples/repository-campaign.yaml")
    parser.add_argument("--modules", default="", help="comma-separated module directories")
    parser.add_argument("--max-directions", type=int)
    parser.add_argument("--top-only", action="store_true")
    parser.add_argument("--skip-passed", action="store_true")
    parser.add_argument("--reuse", action="store_true")
    parser.add_argument("--retry-failed", action="store_true")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--jobs", type=int, help="parallel module analyses")
    parser.add_argument("--parallel-pipelines", type=int,
                        help="isolated implementation worktrees")
    parser.add_argument("--pipeline", default="",
                        help="implementation pipeline template override")
    parser.add_argument("--output-dir", default="")
    parser.add_argument("--state-file", default="")
    parser.add_argument("--summary-file", default="")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--preflight-strict", action="store_true",
        help="abort before direction pipelines when a repo-scoped pipeline "
             "validator is red at HEAD")
    parser.add_argument("--agent-bin", default=config.AGENT_BIN)
    parser.add_argument("--model", default="")
    parser.add_argument("--provider", default="")
    parser.add_argument("--timeout", type=int, default=0)
    parser.add_argument("--retries", type=int, help="analysis retries")
    parser.add_argument("--retry-delay", type=float, default=10)
    parser.add_argument("--log-file", default="logs/full-auto.log")
    parser.add_argument("--stream-output", choices=["auto", "full", "none"],
                        default=config.STREAM_OUTPUT)
    _add_round_args(parser)
    parser.add_argument("--no-lock", action="store_true")
    parser.add_argument(
        "--wait-lock", type=int, default=0, metavar="MINUTES",
        help="queue behind a held lock instead of exiting immediately")
    parser.add_argument("--memory-mode", choices=["auto", "on", "off"],
                        default=config.MEMORY_MODE)
    _add_metering_args(parser)
    return parser


def _add_round_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--rounds", type=int, default=1,
        help="repeat the campaign up to N times (0 = infinite; fingerprint "
             "reuse makes later rounds incremental)")
    parser.add_argument("--round-delay", type=float, default=0,
                        metavar="SECONDS", help="pause between campaign rounds")
    parser.add_argument(
        "--round-commit", action="store_true",
        help="commit all non-ignored changes after each round; commit errors "
             "fail the campaign")


def _add_metering_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--events-file", default="")
    parser.add_argument("--webhook", default="")
    parser.add_argument("--budget-max", "--max-invocations", dest="budget_max",
                        metavar="N", type=int, default=0)
    parser.add_argument("--daily-budget", "--daily-invocations", dest="daily_budget",
                        metavar="N", type=int, default=0)
    parser.add_argument("--daily-state", default="")


def load_settings(root: Path, args: argparse.Namespace) -> CampaignSettings:
    data = {}
    config_path = _campaign_config_path(root, args.config) if args.config else None
    if config_path and config_path.exists():
        if not yaml:
            raise ValueError("campaign config requires PyYAML")
        try:
            loaded = yaml.safe_load(read_text_bounded(
                config_path, config.INPUT_MAX_BYTES, "campaign config")) or {}
        except Exception as exc:
            raise ValueError(f"invalid campaign YAML: {config_path}: {exc}") from exc
        if not isinstance(loaded, dict):
            raise ValueError(f"campaign config must be a mapping: {config_path}")
        data = loaded
    elif args.config:
        raise ValueError(f"campaign config not found: {config_path}")
    settings = CampaignSettings.from_mapping(data)
    _apply_overrides(settings, args)
    settings.validate()
    _validate_runtime_args(args)
    _validate_paths(root, settings)
    return settings


def _validate_runtime_args(args: argparse.Namespace) -> None:
    values = {"timeout": args.timeout, "retry-delay": args.retry_delay,
              "budget-max": args.budget_max, "daily-budget": args.daily_budget,
              "round-delay": args.round_delay}
    invalid = [name for name, value in values.items()
               if not math.isfinite(float(value)) or value < 0]
    if args.rounds < 0:
        invalid.append("rounds")
    if not str(args.agent_bin).strip():
        invalid.append("agent-bin")
    if invalid:
        raise ValueError("invalid campaign runtime values: " + ", ".join(invalid))


def _apply_overrides(settings: CampaignSettings, args: argparse.Namespace) -> None:
    if args.max_directions is not None:
        settings.max_directions = args.max_directions
    if args.top_only:
        settings.max_directions = 1
    if args.jobs is not None:
        settings.analysis_jobs = args.jobs
    if args.parallel_pipelines is not None:
        settings.pipeline_concurrency = args.parallel_pipelines
    if args.retries is not None:
        settings.analysis_retries = args.retries
    values = (("pipeline_template", args.pipeline), ("output_dir", args.output_dir),
              ("state_file", args.state_file), ("summary_file", args.summary_file))
    for attr, value in values:
        if value:
            setattr(settings, attr, value)


# Execution remains in the split module. These aliases keep the historical
# import surface used by integrations and tests.
from .campaign_exec import (  # noqa: E402
    _implement, _register_campaign_run, _round_commit, main, run_campaign)


def _snapshot_excludes(root: Path, settings: CampaignSettings,
                       args: argparse.Namespace) -> tuple[str, ...]:
    from .campaign_exec import _snapshot_excludes as implementation
    return implementation(root, settings, args)


def _refresh_repository_snapshot(context: CampaignContext) -> None:
    excludes = _snapshot_excludes(context.root, context.settings, context.args)
    context.snapshot = git_snapshot(context.root, excludes)


def run_campaign_loop(context: CampaignContext) -> int:
    """Compatibility facade matching the execution-layer round semantics."""
    if context.args.dry_run:
        return run_campaign(context)
    rounds = context.args.rounds
    code = 0
    round_no = 1
    while rounds == 0 or round_no <= rounds:
        label = str(rounds) if rounds else "∞"
        log.info("\n========== CAMPAIGN ROUND %d/%s ==========", round_no, label)
        _refresh_repository_snapshot(context)
        result = run_campaign(context)
        if result and code == 0:
            code = result
        if context.args.round_commit and not _round_commit(context, round_no):
            code = code or 1
        if rounds > 0 and round_no >= rounds:
            break
        if context.args.round_delay > 0:
            time.sleep(context.args.round_delay)
        round_no += 1
    return code
