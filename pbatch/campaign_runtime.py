"""Small runtime-policy helpers for repository campaigns."""

from __future__ import annotations

import argparse
from typing import TYPE_CHECKING

from .pipeline_status import PipelineStatus

if TYPE_CHECKING:
    from .campaign import CampaignContext


def _agent_args(args: argparse.Namespace) -> list[str]:
    values = ["--agent-bin", args.agent_bin, "--memory-mode", args.memory_mode,
              "--stream-output", args.stream_output]
    for flag, value in (("--model", args.model), ("--provider", args.provider),
                        ("--timeout", args.timeout)):
        if value:
            values.extend([flag, str(value)])
    return values


def _reuse_enabled(args: argparse.Namespace) -> bool:
    return not args.force and (args.reuse or args.skip_passed or args.retry_failed)


def _print_plan(context: CampaignContext, modules: list[str]) -> None:
    analysis_sec = context.store.median_elapsed(
        ("ANALYZED",), context.settings.analysis_minutes * 60)
    pipeline_sec = context.store.median_elapsed(
        (PipelineStatus.PASSED, PipelineStatus.GATE_REJECTED,
         PipelineStatus.PIPELINE_FAILED, PipelineStatus.VALIDATION_FAILED),
        context.settings.pipeline_minutes * 60)
    analysis_wall = len(modules) * analysis_sec / context.settings.analysis_jobs
    pipeline_wall = (len(modules) * context.settings.max_directions * pipeline_sec /
                     context.settings.pipeline_concurrency)
    print(f"== Campaign: {context.settings.name} ==")
    print(f"Modules: {len(modules)}; directions/module: {context.settings.max_directions}")
    for module in modules:
        print(f"  - {module}")
    print(f"Estimated analysis: {analysis_wall / 60:.0f} min")
    print(f"Estimated pipelines: {pipeline_wall / 60:.0f} min")
    print(f"Estimated total: {(analysis_wall + pipeline_wall) / 60:.0f} min")
    print("Dry-run made no files or state changes.")
