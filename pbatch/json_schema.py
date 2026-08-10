"""Pure-stdlib, fail-closed JSON Schema subset for artifact gates."""

from __future__ import annotations

import ipaddress
import json
import math
import re
from datetime import date, datetime, time
from decimal import Decimal, InvalidOperation
from pathlib import Path
from urllib.parse import urlparse
from uuid import UUID


DEFAULT_MAX_BYTES = 2 * 1024 * 1024
TYPE_NAMES = {"array", "boolean", "integer", "null", "number", "object", "string"}
FORMATS = {"date", "date-time", "email", "hostname", "ipv4", "ipv6", "time", "uri", "uuid"}
ANNOTATIONS = {
    "$anchor", "$comment", "$id", "$schema", "default", "deprecated",
    "description", "examples", "readOnly", "title", "writeOnly",
}
KEYWORDS = ANNOTATIONS | {
    "$defs", "$ref", "additionalProperties", "allOf", "anyOf", "const",
    "contains", "definitions", "dependentRequired", "dependentSchemas",
    "else", "enum", "exclusiveMaximum", "exclusiveMinimum", "format", "if",
    "items", "maxContains", "maximum", "maxItems", "maxLength",
    "maxProperties", "minContains", "minimum", "minItems", "minLength",
    "minProperties", "multipleOf", "not", "oneOf", "pattern",
    "patternProperties", "prefixItems", "properties", "propertyNames",
    "required", "then", "type", "uniqueItems",
}
MAP_SCHEMAS = {"$defs", "definitions", "dependentSchemas", "patternProperties", "properties"}
LIST_SCHEMAS = {"allOf", "anyOf", "oneOf", "prefixItems"}
SINGLE_SCHEMAS = {
    "additionalProperties", "contains", "else", "if", "items", "not",
    "propertyNames", "then",
}
COUNT_KEYWORDS = {
    "maxContains", "maxItems", "maxLength", "maxProperties", "minContains",
    "minItems", "minLength", "minProperties",
}
NUMBER_KEYWORDS = {
    "exclusiveMaximum", "exclusiveMinimum", "maximum", "minimum", "multipleOf",
}


class SchemaError(ValueError):
    """The schema is malformed or uses constraints this checker cannot enforce."""


def load_json_file(path, label: str, max_bytes: int = DEFAULT_MAX_BYTES):
    """Load bounded, strict JSON and reject non-standard NaN/Infinity values."""
    source = Path(path)
    try:
        size = source.stat().st_size
        if not source.is_file():
            raise SchemaError(f"{label} is not a regular file: {source}")
        if size > max_bytes:
            raise SchemaError(f"{label} exceeds {max_bytes} bytes: {source}")
        text = source.read_text(encoding="utf-8")
        return json.loads(text, parse_constant=_reject_constant)
    except SchemaError:
        raise
    except (OSError, UnicodeError, ValueError, RecursionError) as exc:
        raise SchemaError(f"invalid {label} {source}: {exc}") from exc


def _reject_constant(value: str):
    raise ValueError(f"non-standard JSON constant {value}")


def validate(instance, schema) -> list[str]:
    """Return deterministic validation errors; malformed schemas raise."""
    try:
        _audit_schema(schema, "$schema", set())
        return _validate(instance, schema, schema, "$", set())
    except RecursionError as exc:
        raise SchemaError("schema or artifact nesting exceeds the safe recursion limit") from exc


def _audit_schema(schema, path: str, seen: set[int]) -> None:
    if isinstance(schema, bool):
        return
    if not isinstance(schema, dict):
        raise SchemaError(f"{path} must be an object or boolean")
    if id(schema) in seen:
        return
    seen.add(id(schema))
    unknown = sorted(set(schema) - KEYWORDS)
    if unknown:
        raise SchemaError(f"{path} has unsupported keyword(s): {', '.join(unknown)}")
    _audit_values(schema, path)
    for name, child in _subschemas(schema, path):
        _audit_schema(child, name, seen)


def _audit_values(schema: dict, path: str) -> None:
    _audit_type(schema, path)
    _audit_required(schema, path)
    if "enum" in schema and (not isinstance(schema["enum"], list) or not schema["enum"]):
        raise SchemaError(f"{path}.enum must be a non-empty array")
    _audit_numbers(schema, path)
    _audit_patterns(schema, path)
    _audit_format_and_ref(schema, path)


def _audit_type(schema: dict, path: str) -> None:
    if "type" not in schema:
        return
    declared = schema.get("type")
    types = [declared] if isinstance(declared, str) else declared
    if (
        not isinstance(types, list) or not types
        or any(not isinstance(item, str) or item not in TYPE_NAMES for item in types)
    ):
        raise SchemaError(f"{path}.type must contain supported JSON types")


def _audit_required(schema: dict, path: str) -> None:
    required = schema.get("required", [])
    if not isinstance(required, list) or any(not isinstance(item, str) for item in required):
        raise SchemaError(f"{path}.required must be an array of strings")
    if len(required) != len(set(required)):
        raise SchemaError(f"{path}.required contains duplicate names")


def _audit_format_and_ref(schema: dict, path: str) -> None:
    if "format" in schema and schema["format"] not in FORMATS:
        raise SchemaError(f"{path}.format is unsupported: {schema['format']!r}")
    ref = schema.get("$ref")
    if "$ref" in schema and (not isinstance(ref, str) or not ref.startswith("#")):
        raise SchemaError(f"{path}.$ref must be a local JSON pointer")


def _audit_numbers(schema: dict, path: str) -> None:
    for key in COUNT_KEYWORDS:
        value = schema.get(key)
        if key in schema and (
            not isinstance(value, int) or isinstance(value, bool) or value < 0
        ):
            raise SchemaError(f"{path}.{key} must be a non-negative integer")
    for key in NUMBER_KEYWORDS:
        value = schema.get(key)
        if key in schema and not _is_number(value):
            raise SchemaError(f"{path}.{key} must be a finite number")
    if "multipleOf" in schema and schema["multipleOf"] <= 0:
        raise SchemaError(f"{path}.multipleOf must be greater than zero")
    if "uniqueItems" in schema and not isinstance(schema["uniqueItems"], bool):
        raise SchemaError(f"{path}.uniqueItems must be boolean")


def _audit_patterns(schema: dict, path: str) -> None:
    if "pattern" in schema and not isinstance(schema["pattern"], str):
        raise SchemaError(f"{path}.pattern must be a string")
    patterns = []
    if "pattern" in schema:
        patterns.append(schema["pattern"])
    mapping = schema.get("patternProperties", {})
    if not isinstance(mapping, dict):
        raise SchemaError(f"{path}.patternProperties must be an object")
    patterns.extend(mapping)
    try:
        for pattern in patterns:
            re.compile(pattern)
    except (re.error, TypeError) as exc:
        raise SchemaError(f"{path} contains an invalid regular expression: {exc}") from exc


def _subschemas(schema: dict, path: str):
    for key in MAP_SCHEMAS:
        value = schema.get(key, {})
        if not isinstance(value, dict):
            raise SchemaError(f"{path}.{key} must be an object")
        for name, child in value.items():
            yield f"{path}.{key}.{name}", child
    for key in LIST_SCHEMAS:
        value = schema.get(key, [])
        if not isinstance(value, list) or (key != "prefixItems" and key in schema and not value):
            raise SchemaError(f"{path}.{key} must be a non-empty array of schemas")
        for index, child in enumerate(value):
            yield f"{path}.{key}[{index}]", child
    for key in SINGLE_SCHEMAS:
        if key in schema:
            yield f"{path}.{key}", schema[key]
    _audit_dependencies(schema, path)


def _audit_dependencies(schema: dict, path: str) -> None:
    dependencies = schema.get("dependentRequired", {})
    if not isinstance(dependencies, dict):
        raise SchemaError(f"{path}.dependentRequired must be an object")
    for key, names in dependencies.items():
        if not isinstance(names, list) or any(not isinstance(name, str) for name in names):
            raise SchemaError(f"{path}.dependentRequired.{key} must be a string array")


def _validate(instance, schema, root, path: str, active: set[tuple]) -> list[str]:
    if schema is True:
        return []
    if schema is False:
        return [f"{path}: rejected by false schema"]
    marker = (id(schema), id(instance))
    if marker in active:
        return []
    active.add(marker)
    try:
        return _validate_inner(instance, schema, root, path, active)
    finally:
        active.remove(marker)


def _validate_inner(instance, schema: dict, root, path: str, active: set) -> list[str]:
    errors = []
    if "$ref" in schema:
        target = _resolve_ref(root, schema["$ref"])
        errors.extend(_validate(instance, target, root, path, active))
    errors.extend(_logic_errors(instance, schema, root, path, active))
    if "type" in schema and not _matches_type(instance, schema["type"]):
        return errors + [f"{path}: expected type {schema['type']!r}, got {_type_name(instance)}"]
    if "enum" in schema and not any(_json_equal(instance, item) for item in schema["enum"]):
        errors.append(f"{path}: value is not in enum")
    if "const" in schema and not _json_equal(instance, schema["const"]):
        errors.append(f"{path}: value does not equal const")
    if isinstance(instance, dict):
        errors.extend(_object_errors(instance, schema, root, path, active))
    elif isinstance(instance, list):
        errors.extend(_array_errors(instance, schema, root, path, active))
    elif isinstance(instance, str):
        errors.extend(_string_errors(instance, schema, path))
    elif _is_number(instance):
        errors.extend(_number_errors(instance, schema, path))
    return errors


def _logic_errors(instance, schema: dict, root, path: str, active: set) -> list[str]:
    errors = []
    for child in schema.get("allOf", []):
        errors.extend(_validate(instance, child, root, path, active))
    if "anyOf" in schema:
        matches = sum(not _validate(instance, item, root, path, active)
                      for item in schema["anyOf"])
        if not matches:
            errors.append(f"{path}: no anyOf branch matched")
    if "oneOf" in schema:
        matches = sum(not _validate(instance, item, root, path, active)
                      for item in schema["oneOf"])
        if matches != 1:
            errors.append(f"{path}: expected exactly one oneOf match, got {matches}")
    if "not" in schema and not _validate(instance, schema["not"], root, path, active):
        errors.append(f"{path}: matched forbidden not schema")
    if "if" in schema:
        branch = "then" if not _validate(instance, schema["if"], root, path, active) else "else"
        if branch in schema:
            errors.extend(_validate(instance, schema[branch], root, path, active))
    return errors


def _object_errors(value: dict, schema: dict, root, path: str, active: set) -> list[str]:
    errors = []
    _check_size(value, schema, path, "Properties", errors)
    for name in schema.get("required", []):
        if name not in value:
            errors.append(f"{path}: missing required property {name!r}")
    properties = schema.get("properties", {})
    patterns = [(re.compile(pattern), child)
                for pattern, child in schema.get("patternProperties", {}).items()]
    additional = schema.get("additionalProperties", True)
    for name, item in value.items():
        child_path = _child_path(path, name)
        matched = False
        if name in properties:
            matched = True
            errors.extend(_validate(item, properties[name], root, child_path, active))
        for pattern, child in patterns:
            if pattern.search(name):
                matched = True
                errors.extend(_validate(item, child, root, child_path, active))
        if not matched and additional is False:
            errors.append(f"{child_path}: additional property is not allowed")
        elif not matched and isinstance(additional, dict):
            errors.extend(_validate(item, additional, root, child_path, active))
    errors.extend(_object_dependency_errors(value, schema, root, path, active))
    if "propertyNames" in schema:
        for name in value:
            errors.extend(_validate(name, schema["propertyNames"], root,
                                    f"{path}.<property-name>", active))
    return errors


def _object_dependency_errors(value, schema, root, path, active) -> list[str]:
    errors = []
    for source, required in schema.get("dependentRequired", {}).items():
        if source not in value:
            continue
        for name in required:
            if name not in value:
                errors.append(f"{path}: property {source!r} requires {name!r}")
    for source, child in schema.get("dependentSchemas", {}).items():
        if source in value:
            errors.extend(_validate(value, child, root, path, active))
    return errors


def _array_errors(value: list, schema: dict, root, path: str, active: set) -> list[str]:
    errors = []
    _check_size(value, schema, path, "Items", errors)
    if schema.get("uniqueItems"):
        seen = set()
        for index, item in enumerate(value):
            key = _json_key(item)
            if key in seen:
                errors.append(f"{path}[{index}]: duplicate item")
            seen.add(key)
    prefix = schema.get("prefixItems", [])
    for index, child in enumerate(prefix[:len(value)]):
        errors.extend(_validate(value[index], child, root, f"{path}[{index}]", active))
    if "items" in schema:
        for index in range(len(prefix), len(value)):
            errors.extend(_validate(value[index], schema["items"], root,
                                    f"{path}[{index}]", active))
    if "contains" in schema:
        count = sum(not _validate(item, schema["contains"], root,
                                  f"{path}[{index}]", active)
                    for index, item in enumerate(value))
        minimum = schema.get("minContains", 1)
        maximum = schema.get("maxContains")
        if count < minimum or (maximum is not None and count > maximum):
            errors.append(f"{path}: contains matched {count} item(s), expected {minimum}..{maximum or 'unbounded'}")
    return errors


def _string_errors(value: str, schema: dict, path: str) -> list[str]:
    errors = []
    if len(value) < schema.get("minLength", 0):
        errors.append(f"{path}: string is shorter than minLength")
    maximum = schema.get("maxLength")
    if maximum is not None and len(value) > maximum:
        errors.append(f"{path}: string is longer than maxLength")
    if "pattern" in schema and not re.search(schema["pattern"], value):
        errors.append(f"{path}: string does not match pattern")
    if "format" in schema and not _format_valid(value, schema["format"]):
        errors.append(f"{path}: string is not a valid {schema['format']}")
    return errors


def _number_errors(value, schema: dict, path: str) -> list[str]:
    errors = []
    checks = (
        ("minimum", lambda left, right: left >= right),
        ("maximum", lambda left, right: left <= right),
        ("exclusiveMinimum", lambda left, right: left > right),
        ("exclusiveMaximum", lambda left, right: left < right),
    )
    for key, predicate in checks:
        if key in schema and not predicate(value, schema[key]):
            errors.append(f"{path}: number violates {key}={schema[key]}")
    if "multipleOf" in schema:
        try:
            if Decimal(str(value)) % Decimal(str(schema["multipleOf"])) != 0:
                errors.append(f"{path}: number is not a multipleOf {schema['multipleOf']}")
        except (InvalidOperation, ZeroDivisionError):
            errors.append(f"{path}: number cannot be checked against multipleOf")
    return errors


def _check_size(value, schema: dict, path: str, suffix: str, errors: list) -> None:
    minimum = schema.get(f"min{suffix}")
    maximum = schema.get(f"max{suffix}")
    if minimum is not None and len(value) < minimum:
        errors.append(f"{path}: size is below min{suffix}={minimum}")
    if maximum is not None and len(value) > maximum:
        errors.append(f"{path}: size exceeds max{suffix}={maximum}")


def _resolve_ref(root, ref: str):
    if ref == "#":
        return root
    if not ref.startswith("#/"):
        raise SchemaError(f"unsupported $ref {ref!r}; only local JSON pointers are allowed")
    current = root
    for raw in ref[2:].split("/"):
        token = raw.replace("~1", "/").replace("~0", "~")
        try:
            current = current[int(token)] if isinstance(current, list) else current[token]
        except (KeyError, IndexError, TypeError, ValueError) as exc:
            raise SchemaError(f"unresolved $ref {ref!r}") from exc
    return current


def _matches_type(value, declared) -> bool:
    names = [declared] if isinstance(declared, str) else declared
    return any(_matches_one_type(value, name) for name in names)


def _matches_one_type(value, name: str) -> bool:
    if name == "null":
        return value is None
    if name == "boolean":
        return isinstance(value, bool)
    if name == "object":
        return isinstance(value, dict)
    if name == "array":
        return isinstance(value, list)
    if name == "string":
        return isinstance(value, str)
    if name == "number":
        return _is_number(value)
    if name == "integer":
        return (
            isinstance(value, int) and not isinstance(value, bool)
            or isinstance(value, float) and math.isfinite(value) and value.is_integer()
        )
    return False


def _is_number(value) -> bool:
    return (
        isinstance(value, (int, float)) and not isinstance(value, bool)
        and (not isinstance(value, float) or math.isfinite(value))
    )


def _type_name(value) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, dict):
        return "object"
    if isinstance(value, list):
        return "array"
    if isinstance(value, str):
        return "string"
    return "number" if _is_number(value) else type(value).__name__


def _json_equal(left, right) -> bool:
    if _is_number(left) and _is_number(right):
        return Decimal(str(left)) == Decimal(str(right))
    if type(left) is not type(right):
        return False
    if isinstance(left, list):
        return len(left) == len(right) and all(
            _json_equal(a, b) for a, b in zip(left, right)
        )
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(
            _json_equal(left[key], right[key]) for key in left
        )
    return left == right


def _json_key(value):
    """Hashable JSON equality key used by uniqueItems in linear time."""
    if _is_number(value):
        return "number", Decimal(str(value))
    if isinstance(value, list):
        return "array", tuple(_json_key(item) for item in value)
    if isinstance(value, dict):
        return "object", tuple(sorted(
            (name, _json_key(item)) for name, item in value.items()
        ))
    return _type_name(value), value


def _child_path(path: str, name: str) -> str:
    return f"{path}.{name}" if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) else f"{path}[{name!r}]"


def _format_valid(value: str, name: str) -> bool:
    try:
        if name == "date":
            date.fromisoformat(value)
        elif name == "time":
            time.fromisoformat(value.replace("Z", "+00:00"))
        elif name == "date-time":
            if "T" not in value and "t" not in value:
                return False
            datetime.fromisoformat(value.replace("Z", "+00:00"))
        elif name == "email":
            return bool(re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", value))
        elif name == "hostname":
            return bool(re.fullmatch(r"(?=.{1,253}$)(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?", value))
        elif name == "ipv4":
            return isinstance(ipaddress.ip_address(value), ipaddress.IPv4Address)
        elif name == "ipv6":
            return isinstance(ipaddress.ip_address(value), ipaddress.IPv6Address)
        elif name == "uri":
            return bool(urlparse(value).scheme)
        elif name == "uuid":
            UUID(value)
        return True
    except (ValueError, TypeError):
        return False
