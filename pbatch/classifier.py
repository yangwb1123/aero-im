"""Task type classifier: a deterministic, bilingual keyword gate that runs
BEFORE execution and decides whether a task/batch is frontend, backend, or
generic work, so the CLI can route confident domain majorities to their
implementation pipelines (--classify flag or the `classify` subcommand).

Scoring mirrors pbatch.relevance: CJK terms match by substring, Latin terms
by word boundaries, 2 points per hit, capped at 10 per type. A platform
hit (tsx/dart/vue/react-native) adds a +2 boost to frontend_ui because
"implement this page in flutter" is overwhelmingly a UI task even when the
rest of the sentence is business wording. Profile hits (erp/cms/oa/
dashboard/immersive/marketing/mobile) add +1 each (cap +4): "a marketing
landing page" is a UI signal even with no generic UI word.

Zero-score input classifies as unknown. Batch routing requires a confident
majority; ties follow the configured policy (frontend by default for backward
compatibility). An explicit --pipeline always wins.
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from . import config
from .classifier_keywords import _DEFAULT_KEYWORDS, _SYSTEM_TYPE_TERMS
from .config import log, yaml
from .relevance import _keyword_hit
from .text_io import read_text_bounded

UNKNOWN = "unknown"
FRONTEND = "frontend_ui"
BACKEND = "backend"
PLATFORM_BOOST = 2

@dataclass(frozen=True)
class TaskClassification:
    """One task's classification: dominant task type plus frontend
    platform/profile detail and the keyword evidence behind the score."""

    task_type: str
    score: int
    matched: tuple = ()
    platform: str = UNKNOWN
    profile: str = UNKNOWN
    confident: bool = False

    # 问题系统分类学（哥德尔启发）：prompt 归为哪类系统 → 方法论路由。
    # state-machine / event-driven / realtime / search / optimization /
    # knowledge / batch / adaptive / collaboration / deterministic
    system_type: str = "deterministic"
    system_evidence: tuple = ()

    def to_dict(self) -> dict:
        return {
            "task_type": self.task_type,
            "score": self.score,
            "matched": list(self.matched),
            "platform": self.platform,
            "profile": self.profile,
            "confident": self.confident,
            "system_type": self.system_type,
            "system_evidence": list(self.system_evidence),
        }


def _default_keyword_paths() -> list[Path]:
    return [Path(__file__).with_name("task_keywords.yaml")]


def _keyword_file(path: Path, strict: bool) -> dict:
    if not path.is_file() or path.is_symlink():
        if strict:
            raise ValueError(f"task keyword file not found or unsafe: {path}")
        return {}
    if not yaml:
        if strict:
            raise ValueError("task keyword YAML requires PyYAML")
        return {}
    try:
        data = yaml.safe_load(read_text_bounded(
            path, config.INPUT_MAX_BYTES, "task keyword file")) or {}
    except Exception as exc:
        if strict:
            raise ValueError(f"invalid task keyword file: {exc}") from exc
        return {}
    if not isinstance(data, dict):
        if strict:
            raise ValueError("task keyword file must contain a mapping")
        return {}
    return data


def _keyword_schema_error(data: dict, allowed: set[str]) -> str:
    unknown = sorted(set(data) - allowed)
    if unknown:
        return "unknown section(s): " + ", ".join(unknown)
    if not data:
        return "at least one keyword section is required"
    for section, section_map in data.items():
        if not isinstance(section_map, dict) or not section_map:
            return f"section '{section}' must be a non-empty mapping"
        for name, terms in section_map.items():
            if not isinstance(name, str) or not name.strip():
                return f"section '{section}' has an invalid name"
            if not isinstance(terms, list) or not terms or not all(
                    isinstance(term, str) and term.strip() for term in terms):
                return f"section '{section}.{name}' must be a non-empty string list"
    return ""


def _merge_keyword_sections(merged: dict, data: dict) -> None:
    for section, section_map in data.items():
        if section not in merged or not isinstance(section_map, dict):
            continue
        for name, terms in section_map.items():
            current = merged[section].setdefault(name, [])
            current.extend(str(term) for term in terms if str(term) not in current)


def load_keywords(path: str = "") -> dict:
    """Load the keyword map: YAML entries are ADDITIVE to the built-in
    defaults (per type list, deduped) so projects can extend vocabulary
    without rewriting the full map; malformed/absent files degrade to
    defaults."""
    merged = {section: {name: list(terms) for name, terms in section_map.items()}
              for section, section_map in _DEFAULT_KEYWORDS.items()}
    candidates = [Path(path)] if path else _default_keyword_paths()
    for candidate in candidates:
        data = _keyword_file(candidate, bool(path))
        if path:
            error = _keyword_schema_error(data, set(merged))
            if error:
                raise ValueError(f"invalid task keyword file: {error}")
        _merge_keyword_sections(merged, data)
    return merged


def _best_match(text: str, section: dict, lowered: str = "") -> tuple[str, tuple]:
    """Best-scoring entry in a keyword section (platform/profile maps)."""
    best, best_hits = UNKNOWN, ()
    for name, terms in section.items():
        hits = tuple(str(term) for term in terms if _keyword_hit(lowered, str(term)))
        if len(hits) > len(best_hits):
            best, best_hits = name, hits
    return best, best_hits


def _frontend_boost(platform: str, platform_hits: tuple,
                    profile: str, profile_hits: tuple) -> int:
    """Evidence boost for the frontend type: a platform mention (tsx/dart/
    flutter/vue/rn, +2) and profile hits (erp/cms/oa/..., +1 each, cap +4)."""
    boost = 0
    if platform != UNKNOWN and platform_hits:
        boost += PLATFORM_BOOST
    if profile != UNKNOWN and profile_hits:
        boost += min(4, len(profile_hits))
    return boost


def _detect_system_type(text: str, keywords: Optional[dict] = None) -> tuple:
    """哥德尔启发：先判定问题属于哪类系统，再选方法论。零 LLM 成本。"""
    lowered = (text or "").lower()
    best_name = "deterministic"
    best_score = 0
    best_hits: list[str] = []
    terms = (keywords or {}).get("system_types", _SYSTEM_TYPE_TERMS)
    for name, wordlist in terms.items():
        hits = [str(t) for t in wordlist if _keyword_hit(lowered, str(t))]
        if len(hits) > best_score:
            best_score = len(hits)
            best_name = name
            best_hits = hits
    return best_name, tuple(best_hits[:5])


def _score_task_types(lowered: str, kw: dict) -> tuple:
    types = kw.get("task_types", {})
    platforms = kw.get("platforms", {})
    profiles = kw.get("profiles", {})
    scored = []
    for name, terms in types.items():
        hits = tuple(str(term) for term in terms if _keyword_hit(lowered, str(term)))
        scored.append((name, min(10, len(hits) * 2), hits))
    platform, platform_hits = _best_match(lowered, platforms, lowered)
    profile, profile_hits = _best_match(lowered, profiles, lowered)
    boost = _frontend_boost(platform, platform_hits, profile, profile_hits)
    if boost:
        scored = [(n, s + (boost if n == FRONTEND else 0), h)
                  for n, s, h in scored]
    scored.sort(key=lambda item: (-item[1], 0 if item[0] == FRONTEND else 1, item[0]))
    return scored, platform, platform_hits, profile, profile_hits


def _winning_score(scored: list, system_type: str, system_evidence: tuple):
    best_type, best_score, best_hits = scored[0]
    if best_score <= 0:
        return TaskClassification(UNKNOWN, 0, system_type=system_type,
                                   system_evidence=system_evidence)
    tied = [item for item in scored if item[1] == best_score]
    if len(tied) == 1:
        return best_type, best_score, best_hits
    best_type = _resolve_type_tie([item[0] for item in tied])
    if best_type != UNKNOWN:
        return next(item for item in tied if item[0] == best_type)
    hits = tuple(dict.fromkeys(hit for _, _, item_hits in tied for hit in item_hits))
    return TaskClassification(
        UNKNOWN, best_score, matched=hits, confident=False,
        system_type=system_type, system_evidence=system_evidence)


def classify_text(text: str, keywords: Optional[dict] = None) -> TaskClassification:
    """Classify one prompt and retain the evidence behind its top score."""
    kw = keywords or load_keywords()
    lowered = (text or "").lower()
    scored, platform, platform_hits, profile, profile_hits = _score_task_types(
        lowered, kw)
    system_type, system_evidence = _detect_system_type(text, kw)
    winner = _winning_score(scored, system_type, system_evidence)
    if isinstance(winner, TaskClassification):
        return winner
    best_type, best_score, best_hits = winner

    matched = best_hits
    if best_type == FRONTEND:
        # Evidence transparency: include the platform/profile hits that
        # contributed the frontend boost.
        matched = best_hits + platform_hits + profile_hits
    return TaskClassification(
        task_type=best_type,
        score=best_score,
        matched=matched,
        platform=platform if best_type == FRONTEND else UNKNOWN,
        profile=profile if best_type == FRONTEND else UNKNOWN,
        confident=best_score >= config.CLASSIFIER_MIN_SCORE,
        system_type=system_type,
        system_evidence=system_evidence,
    )


def _resolve_type_tie(types: list[str], policy: str = "") -> str:
    """Resolve an ambiguous classification using the configured policy."""
    tied = set(types)
    selected = policy or config.CLASSIFIER_TIE_POLICY
    if selected == "frontend" and FRONTEND in tied:
        return FRONTEND
    if selected == "backend" and BACKEND in tied:
        return BACKEND
    return UNKNOWN


def classify_tasks(tasks: list, keywords: Optional[dict] = None) -> tuple:
    """Classify a whole batch; returns (dominant, per_task). The dominant
    type is the majority of per-task top types; ambiguous ties are unknown
    unless classifier.tie_policy explicitly selects frontend or backend."""
    per_task = []
    for t in tasks:
        if isinstance(t, TaskClassification):
            per_task.append(t)
        else:
            per_task.append(classify_text(getattr(t, "prompt", "") or "", keywords))
    counts: dict = {}
    for item in per_task:
        counts[item.task_type] = counts.get(item.task_type, 0) + 1
    best_count = max(counts.values()) if counts else 0
    tied = sorted(name for name, count in counts.items() if count == best_count)
    dominant_type = tied[0] if len(tied) == 1 else _resolve_type_tie(tied)
    dominant = next((item for item in per_task if item.task_type == dominant_type),
                    per_task[0] if per_task else TaskClassification(UNKNOWN, 0))
    return dominant, per_task


def routing_target(per_task: list, frontend_ratio: Optional[float] = None,
                   backend_ratio: Optional[float] = None,
                   tie_policy: str = "") -> str:
    """Return the pipeline domain only for a confident routing majority."""
    if not per_task:
        return UNKNOWN
    counts: dict[str, int] = {}
    for item in per_task:
        if item.confident:
            counts[item.task_type] = counts.get(item.task_type, 0) + 1
    if not counts:
        return UNKNOWN
    thresholds = {
        FRONTEND: (config.CLASSIFIER_FRONTEND_RATIO
                   if frontend_ratio is None else frontend_ratio),
        BACKEND: (config.CLASSIFIER_BACKEND_RATIO
                  if backend_ratio is None else backend_ratio),
    }
    largest = max(counts.values())
    candidates = [task_type for task_type, threshold in thresholds.items()
                  if counts.get(task_type, 0) == largest
                  and counts.get(task_type, 0) / len(per_task) >= threshold]
    if len(candidates) == 1:
        return candidates[0]
    return _resolve_type_tie(candidates, tie_policy) if candidates else UNKNOWN


def should_route_frontend(per_task: list, min_ratio: float = 0.5) -> bool:
    """Frontend routing decision: at least min_ratio of the batch's
    CONFIDENT top types are frontend_ui (a zero-score best guess must not
    divert a task into the UI pipeline)."""
    return routing_target(per_task, frontend_ratio=min_ratio) == FRONTEND


def should_route_backend(per_task: list, min_ratio: float = 0.5) -> bool:
    """Backward-compatible convenience wrapper for backend routing."""
    return routing_target(per_task, backend_ratio=min_ratio) == BACKEND


def format_classification(item: TaskClassification, index: int = 0) -> str:
    """One human-readable classification line with evidence."""
    label = f"task {index}: " if index else ""
    detail = ""
    if item.task_type == FRONTEND and (item.platform != UNKNOWN or item.profile != UNKNOWN):
        detail = " [%s/%s]" % (item.platform, item.profile)
    evidence = ", ".join(item.matched) or "-"
    flag = "confident" if item.confident else "best-guess"
    return f"{label}{item.task_type}{detail} (score {item.score}, {flag}; matched: {evidence})"


def _texts_from_file(src: Path) -> list:
    """Extract prompts from a YAML/JSON task file (tasks list) or treat
    the whole file as one prompt text."""
    text = read_text_bounded(src, config.INPUT_MAX_BYTES, "classify source")
    suffix = src.suffix.lower()
    if suffix in (".yaml", ".yml"):
        if not yaml:
            raise ValueError("structured classifier input requires PyYAML")
        try:
            data = yaml.safe_load(text) or {}
        except Exception as exc:
            raise ValueError(f"invalid classifier YAML: {exc}") from exc
        return _task_prompts_strict(data)
    if suffix == ".json":
        try:
            data = json.loads(text)
        except json.JSONDecodeError as exc:
            raise ValueError(f"invalid classifier JSON: {exc}") from exc
        return _task_prompts_strict(data)
    return [text]


def _task_prompts(data) -> list:
    """Prompts from a parsed task-file payload ({'tasks': [{prompt: ...}]})."""
    if not isinstance(data, dict):
        return []
    tasks = data.get("tasks", [])
    if not isinstance(tasks, list):
        return []
    return [str(t.get("prompt", "")) for t in tasks if isinstance(t, dict) and t.get("prompt")]


def _task_prompts_strict(data) -> list[str]:
    if not isinstance(data, dict) or not isinstance(data.get("tasks"), list):
        raise ValueError("classifier task file must contain a tasks list")
    tasks = data["tasks"]
    if not tasks:
        raise ValueError("classifier task file must contain a non-empty tasks list")
    prompts = _task_prompts(data)
    if len(prompts) != len(tasks) or not all(prompt.strip() for prompt in prompts):
        raise ValueError("every classifier task must have a non-empty prompt")
    return prompts


def _texts_from_dir(directory: Path, suffix: str) -> list:
    if not directory.is_dir():
        raise ValueError(f"classifier source is not a directory: {directory}")
    texts = []
    for p in sorted(directory.glob("*" + (suffix or ".md"))):
        texts.append(read_text_bounded(p, config.INPUT_MAX_BYTES,
                                       "classify source"))
    return texts


def _collect_texts(args) -> list[str]:
    """Gather prompt texts from a subcommand invocation without executing."""
    if args.prompt:
        return [args.prompt]
    if args.from_dir:
        return _texts_from_dir(Path(args.from_dir), args.suffix)
    if args.source:
        src = Path(args.source)
        if not src.exists():
            # Not a file on disk: treat as an inline prompt unless it looks
            # like a path (slash, dot-prefix, or a known file extension).
            looks_like_path = (" " not in args.source
                               and ("/" in args.source or "\\" in args.source
                                   or args.source.startswith(".")
                                   or Path(args.source).suffix in (".md", ".txt", ".yaml", ".yml", ".json")))
            if not looks_like_path:
                return [args.source]
            log.error("File not found: %s", src)
            sys.exit(1)
        return _texts_from_file(src)
    return []


def classify_main(argv: list) -> None:
    """`pi-batch.py classify [prompt|tasks.yaml|--from-dir DIR] [--json]`
    — print the classification and routing decision without executing."""
    import argparse
    parser = argparse.ArgumentParser(
        prog="pi-batch.py classify",
        description="Classify frontend, backend, or generic tasks and show pipeline routing.")
    parser.add_argument("source", nargs="?", help="prompt text, or a YAML/JSON/txt task file path")
    parser.add_argument("-p", "--prompt", help="inline prompt")
    parser.add_argument("--from-dir", dest="from_dir", help="classify every file in DIR")
    parser.add_argument("--suffix", default=".md", help="file suffix for --from-dir (default: .md)")
    parser.add_argument("--json", action="store_true", help="machine-readable JSON output")
    parser.add_argument("--index", default="",
                        help="additive task-keywords YAML extension")
    args = parser.parse_args(argv)
    try:
        texts = _collect_texts(args)
        keywords = load_keywords(args.index)
    except ValueError as exc:
        parser.error(str(exc))
    if not texts:
        parser.error("Provide a prompt (-p), a task file path, or --from-dir")
    per_task = [classify_text(text, keywords) for text in texts]
    dominant = classify_tasks(per_task, keywords)[0]
    target = routing_target(per_task)
    route = target == FRONTEND
    route_backend = target == BACKEND
    if args.json:
        payload = {
            "dominant": dominant.to_dict(),
            "route_frontend": route,
            "route_backend": route_backend,
            "route_pipeline": (config.CLASSIFIER_FRONTEND_PIPELINE if route else
                               config.CLASSIFIER_BACKEND_PIPELINE if route_backend else ""),
            "frontend_pipeline": config.CLASSIFIER_FRONTEND_PIPELINE if route else "",
            "tasks": [item.to_dict() for item in per_task],
        }
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        return
    for index, item in enumerate(per_task, 1):
        print(format_classification(item, index))
    print(f"dominant: {dominant.task_type}; route target: {target}")
    print(f"route to frontend pipeline: {route}")
    print(f"route to backend pipeline: {route_backend}")
