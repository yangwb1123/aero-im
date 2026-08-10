"""Compile declarative structured-artifact validator syntax."""

from __future__ import annotations

import shlex
from pathlib import Path
from typing import Optional


SCHEMA_PREFIX = "jsonschema="


def schema_validator_command(value: str) -> Optional[str]:
    """Return a portable checker command for ``jsonschema=PATH`` values."""
    if not value.startswith(SCHEMA_PREFIX):
        return None
    schema_path = value[len(SCHEMA_PREFIX):].strip()
    checker = Path(__file__).with_name("json_schema_cli.py").resolve()
    return (
        f"python {shlex.quote(str(checker))} --schema "
        f"{shlex.quote(schema_path)} {{output}}"
    )
