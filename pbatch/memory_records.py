"""Bounded session decoding and secret-safe memory record helpers."""

from __future__ import annotations

import json
import re
from pathlib import Path

from . import config
from .memory_io import bounded_lines
from .memory_policy import classify_prompt

_SECRET_NAME = (r"(?:api[_-]?key|access[_-]?token|refresh[_-]?token|token|"
                r"password|passwd|secret(?:[_-]?access[_-]?key)?|"
                r"client[_-]?secret|private[_-]?key|credentials?|authorization)")
_SECRET_KEY_RE = re.compile(rf"(?i)^{_SECRET_NAME}$")
_SECRET_RE = re.compile(
    rf"(?i)[\"']?(?P<key>{_SECRET_NAME})[\"']?\s*[:=]\s*"
    r"(?:[\"'][^\"'\r\n]*[\"']|[^\s,;}\]]+)")
_AUTH_SCHEME_RE = re.compile(r"(?i)\b(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]+")
_KNOWN_SECRET_RE = re.compile(
    r"\b(?:sk-(?:proj-)?[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{20,}|"
    r"AKIA[A-Z0-9]{16})\b")
_PRIVATE_KEY_RE = re.compile(
    r"-----BEGIN [^-\r\n]*PRIVATE KEY-----.*?-----END [^-\r\n]*PRIVATE KEY-----",
    re.DOTALL)


def redact(value: str) -> str:
    value = _PRIVATE_KEY_RE.sub("[REDACTED PRIVATE KEY]", value)
    value = _AUTH_SCHEME_RE.sub("Authorization [REDACTED]", value)
    value = _SECRET_RE.sub(lambda match: f"{match.group('key')}=[REDACTED]", value)
    return _KNOWN_SECRET_RE.sub("[REDACTED]", value)


def redact_content(content):
    if isinstance(content, str):
        return redact(content)
    if isinstance(content, (list, tuple)):
        return [redact_content(item) for item in content]
    if isinstance(content, dict):
        return {key: ("[REDACTED]" if _SECRET_KEY_RE.fullmatch(str(key))
                      else redact_content(value))
                for key, value in content.items()}
    return content


def redact_index_event(event: dict) -> dict:
    """Redact index metadata while retaining paths required for raw reads."""
    protected = {key: event[key] for key in ("raw_session", "cwd") if key in event}
    payload = redact_content({key: value for key, value in event.items()
                              if key not in protected})
    payload.update(protected)
    return payload


def message_view(line: str, redact_values: bool = True) -> dict | None:
    try:
        item = json.loads(line)
    except ValueError:
        return None
    if item.get("type") != "message" or not isinstance(item.get("message"), dict):
        return None
    message = item["message"]
    view = {"role": message.get("role", ""), "content": message.get("content", ""),
            "usage": message.get("usage")}
    return redact_content(view) if redact_values else view


def session_entry(path: Path, size: int, mtime_ns: int) -> dict:
    summary = {"session_id": "", "session_name": "", "cwd": "",
               "roles": {}, "message_count": 0, "total_tokens": 0, "cost": 0.0,
               "user_text": "", "observed_verdict": "", "observed_failure": ""}
    try:
        for line in bounded_lines(path, config.SESSION_LINE_MAX_BYTES):
            if line is not None:
                _fold_session_line(summary, line)
    except OSError:
        pass
    profile = classify_prompt(summary["user_text"])
    verdict = summary["observed_verdict"]
    failure = summary["observed_failure"]
    status = "FAILED_OBSERVED" if failure else (
        "GATE_PASS_OBSERVED" if verdict == "PASS" else "IMPORTED")
    if not failure and verdict in ("FAIL", "REJECT"):
        status = "GATE_REJECTED_OBSERVED"
    return {"type": "session_import", "session_id": summary["session_id"],
            "session_name": summary["session_name"], "status": status,
            "mode": profile["mode"], "domains": profile["domains"],
            "prompt_excerpt": redact(" ".join(summary["user_text"].split())[:240]),
            "message_count": summary["message_count"], "roles": summary["roles"],
            "total_tokens": summary["total_tokens"], "cost": summary["cost"],
            "observed_verdict": verdict, "observed_failure": redact(failure),
            "reason": redact(failure), "raw_session": str(path.resolve()),
            "raw_size": size, "raw_mtime_ns": mtime_ns, "cwd": summary["cwd"]}


def _fold_session_line(summary: dict, line: str) -> None:
    try:
        item = json.loads(line)
    except ValueError:
        return
    if item.get("type") == "session":
        summary["session_id"] = str(item.get("id", summary["session_id"]))[:256]
        summary["cwd"] = str(item.get("cwd", summary["cwd"]))[:1024]
    if item.get("type") == "session_info":
        summary["session_name"] = str(item.get("name", ""))[:256]
    message = item.get("message")
    if item.get("type") != "message" or not isinstance(message, dict):
        _fold_observed_failure(summary, item)
        return
    _fold_observed_failure(summary, message)
    role = str(message.get("role", "unknown"))[:64]
    summary["message_count"] += 1
    _fold_role(summary, role)
    text = _content_text(message.get("content"))
    if role == "user" and len(summary["user_text"]) < 8192:
        summary["user_text"] = (summary["user_text"] + "\n" + text)[:8192]
    _fold_usage(summary, message.get("usage"))
    if role == "assistant":
        _fold_verdict(summary, text)


def _fold_verdict(summary: dict, text: str) -> None:
    verdicts = [value.upper() for value in re.findall(
        r"(?im)^\s*VERDICT\s*:\s*(PASS|FAIL|REJECT)\b", text)]
    if "REJECT" in verdicts:
        summary["observed_verdict"] = "REJECT"
    elif "FAIL" in verdicts:
        summary["observed_verdict"] = "FAIL"
    elif "PASS" in verdicts and summary["observed_verdict"] not in ("FAIL", "REJECT"):
        summary["observed_verdict"] = "PASS"


def _fold_observed_failure(summary: dict, message: dict) -> None:
    if summary["observed_failure"]:
        return
    error = message.get("errorMessage")
    if isinstance(error, str) and error.strip():
        summary["observed_failure"] = error.strip()[:500]
    elif message.get("stopReason") == "error":
        summary["observed_failure"] = "stopReason=error"


def _fold_role(summary: dict, role: str) -> None:
    roles = summary["roles"]
    if role in roles:
        roles[role] += 1
    elif len(roles) < 32:
        roles[role] = 1


def _content_text(content) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "\n".join(item["text"] for item in content
                     if isinstance(item, dict) and isinstance(item.get("text"), str))


def _fold_usage(summary: dict, usage) -> None:
    if not isinstance(usage, dict):
        return
    total = usage.get("totalTokens", usage.get("total", 0))
    if isinstance(total, (int, float)):
        summary["total_tokens"] += total
    cost = usage.get("cost", {})
    value = cost.get("total", 0) if isinstance(cost, dict) else 0
    if isinstance(value, (int, float)):
        summary["cost"] += value
