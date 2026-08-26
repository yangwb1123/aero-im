#!/usr/bin/env python3
"""Create the local tenant/source/schema contract in Audit Governance.

The controller is intentionally create-only. HTTP 409 is accepted as an
idempotent success; any other non-2xx response fails the one-shot container.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


TENANT = "platform-local"


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"missing {name}")
    return value


def request_json(
    url: str,
    *,
    method: str = "GET",
    token: str | None = None,
    body: dict[str, object] | None = None,
) -> tuple[int, dict[str, object]]:
    data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("Accept", "application/json")
    if body is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            raw = response.read(65536)
            return response.status, json.loads(raw or b"{}")
    except urllib.error.HTTPError as error:
        raw = error.read(65536)
        try:
            value = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            value = {}
        return error.code, value


def wait_ready(base_url: str) -> None:
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        try:
            status, _ = request_json(f"{base_url}/readyz")
            if status == 200:
                return
        except (OSError, RuntimeError, urllib.error.URLError):
            pass
        time.sleep(1)
    raise RuntimeError("Audit Governance did not become ready")


def mint_token(token_url: str, client_id: str, client_secret: str) -> str:
    form = urllib.parse.urlencode(
        {
            "grant_type": "client_credentials",
            "scope": "audit:platform:cross_tenant audit:policy:read audit:policy:write",
            "resource": "audit-governance",
        }
    ).encode()
    request = urllib.request.Request(token_url, data=form, method="POST")
    credential = base64.b64encode(f"{client_id}:{client_secret}".encode()).decode()
    request.add_header("Authorization", f"Basic {credential}")
    request.add_header("Content-Type", "application/x-www-form-urlencoded")
    request.add_header("Accept", "application/json")
    with urllib.request.urlopen(request, timeout=5) as response:
        value = json.load(response)
    token = str(value.get("access_token", ""))
    if not token:
        raise RuntimeError("Snaplink returned no bootstrap access token")
    return token


def source_sha(prefix: str, tenant: str) -> str:
    digest = hashlib.sha256(tenant.encode()).digest()
    return f"{prefix}.{base64.urlsafe_b64encode(digest).rstrip(b'=').decode()}"


def vault_source(tenant: str, key: str) -> str:
    message = b"\0".join(
        [b"aero-vault/audit-governance/v1", tenant.encode(), b"source-system", tenant.encode(), b""]
    )
    digest = hmac.new(key.encode(), message, hashlib.sha256).digest()
    return f"aero-vault.{base64.urlsafe_b64encode(digest).rstrip(b'=').decode()}"


def create(base_url: str, token: str, path: str, body: dict[str, object]) -> None:
    status, value = request_json(
        f"{base_url}{path}", method="POST", token=token, body=body
    )
    if status not in (200, 201, 202, 409):
        code = value.get("code", value.get("error", "unknown"))
        raise RuntimeError(f"bootstrap {path} failed: HTTP {status} ({code})")


def main() -> int:
    audit_base = required("AUDIT_BASE_URL").rstrip("/")
    wait_ready(audit_base)
    token = mint_token(
        required("SNAPLINK_TOKEN_URL"),
        required("BOOTSTRAP_CLIENT_ID"),
        required("BOOTSTRAP_CLIENT_SECRET"),
    )

    create(
        audit_base,
        token,
        "/api/v1/tenants",
        {
            "id": TENANT,
            "name": "Aero local platform",
            "home_region": "local",
            "data_region": "local",
            "active": True,
            "events_per_second": 1000,
            "burst": 2000,
        },
    )

    sources = [
        (source_sha("aero-id", TENANT), "Aero ID", ["aero-id-audit"]),
        ("aero-im.source", "Aero IM", ["aero-im-audit"]),
        (
            vault_source(TENANT, required("AERO_VAULT_AUDIT_HMAC_KEY")),
            "Aero Vault",
            ["aero-vault-audit"],
        ),
    ]
    for source_id, name, clients in sources:
        create(
            audit_base,
            token,
            "/api/v1/sources",
            {
                "id": source_id,
                "tenant_id": TENANT,
                "name": name,
                "allowed_client_ids": clients,
                "active": True,
            },
        )

    schemas = [
        ("aero.id.audit-fact", "personal", [], []),
        ("aero.im.security", "confidential", [], []),
        (
            "aero.vault.security",
            "confidential",
            ["fact_kind"],
            [
                "detail_sha256",
                "fact_kind",
                "object_size_bytes",
                "request_id",
                "storage_backend",
            ],
        ),
    ]
    for schema_id, classification, required_fields, allowed_fields in schemas:
        create(
            audit_base,
            token,
            "/api/v1/schemas",
            {
                "tenant_id": TENANT,
                "schema_id": schema_id,
                "version": 1,
                "event_type": schema_id,
                "required_fields": required_fields,
                "allowed_fields": allowed_fields,
                "encrypted_fields": [],
                "searchable_fields": [],
                "classification": classification,
                "active": True,
            },
        )

    print("audit bootstrap complete", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - one-shot controller boundary
        print(f"audit bootstrap failed: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1)
