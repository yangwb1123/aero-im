"""Decision-log formatting helpers extracted from pipeline execution."""

from __future__ import annotations

import re
from datetime import datetime
from pathlib import Path
from typing import Optional


def extract_decisions(text: str, limit: int = 5) -> list[str]:
    """Return markdown decision headings with their first content line."""
    out = []
    lines = (text or "").splitlines()
    for index, line in enumerate(lines):
        if not re.match(r"^#{2,3}\s+", line):
            continue
        heading = line.lstrip("# ").strip()
        following = next((item.strip()[:120]
                          for item in lines[index + 1:index + 4]
                          if item.strip() and not item.lstrip().startswith("#")), "")
        out.append(f"{heading}: {following}" if following else heading)
        if len(out) >= limit:
            break
    return out


def append_decision_log(path: str, stage_name: str, results: list,
                        stage_ok: bool, verdict: Optional[str]) -> None:
    """Append one structured decision record for a completed stage."""
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("a", encoding="utf-8") as stream:
        status = "PASS" if stage_ok else "FAIL"
        timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        stream.write(f"\n## {timestamp} — stage '{stage_name}' — {status}")
        if verdict:
            stream.write(f" (gate verdict: {verdict})")
        stream.write("\n")
        for result in results:
            decisions = extract_decisions(result.stdout)
            label = result.task.output or "(stdout)"
            result_status = "ok" if result.success else f"FAILED: {result.reason}"
            stream.write(f"- task {label} [{result_status}]")
            if decisions:
                stream.write(": " + "; ".join(decisions))
            stream.write("\n")
        evidence = [result.task.output for result in results if result.task.output]
        if evidence:
            stream.write(f"- evidence: {', '.join(evidence)}\n")
