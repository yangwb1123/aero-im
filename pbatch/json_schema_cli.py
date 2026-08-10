"""CLI adapter for the portable structured-artifact schema gate."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pbatch.json_schema import DEFAULT_MAX_BYTES, SchemaError, load_json_file, validate


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate a pure JSON artifact against a portable JSON Schema subset",
    )
    parser.add_argument("artifact", help="pure JSON artifact to validate")
    parser.add_argument("--schema", required=True, help="JSON Schema file")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES,
                        help="maximum bytes for artifact and schema")
    parser.add_argument("--max-errors", type=int, default=20,
                        help="maximum validation errors to report")
    return parser


def _payload(status: str, artifact: str, schema: str, errors: list[str]) -> dict:
    return {
        "status": status,
        "validator": "jsonschema",
        "artifact": artifact,
        "schema": schema,
        "errors": errors,
    }


def main(argv=None) -> int:
    args = _parser().parse_args(argv)
    if args.max_bytes <= 0 or args.max_errors <= 0:
        print("JSONSCHEMA: max limits must be positive", file=sys.stderr)
        return 2
    try:
        schema = load_json_file(args.schema, "schema", args.max_bytes)
        artifact = load_json_file(args.artifact, "artifact", args.max_bytes)
        errors = validate(artifact, schema)[:args.max_errors]
    except SchemaError as exc:
        errors = [str(exc)]
        print(json.dumps(_payload("fail", args.artifact, args.schema, errors)))
        print(f"JSONSCHEMA: {exc}", file=sys.stderr)
        return 2
    status = "fail" if errors else "pass"
    print(json.dumps(_payload(status, args.artifact, args.schema, errors)))
    for error in errors:
        print(f"JSONSCHEMA {error}", file=sys.stderr)
    if errors:
        print(f"JSONSCHEMA: {len(errors)} violation(s); rejected", file=sys.stderr)
        return 1
    print("JSONSCHEMA: OK", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
