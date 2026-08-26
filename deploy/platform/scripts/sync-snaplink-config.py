#!/usr/bin/env python3
"""Converge mutable Snaplink records that static SQLite seeds do not update.

Snaplink's configured clients are create-only seeds. This one-shot controller
uses the supported admin API so an existing local data volume receives later
scope changes without deleting identity or session state.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request


ADMIN_CLIENT_ID = "sso-admin-console"
ADMIN_SCOPES = ["openid", "profile", "admin:read", "admin:write"]
MUTABLE_CLIENT_FIELDS = (
    "id",
    "name",
    "redirectUris",
    "loginPageUri",
    "allowedScopes",
    "allowedAuthenticators",
    "tokenStrategy",
    "active",
    "clientSecretExpiresAt",
)


def required(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise RuntimeError(f"missing {name}")
    return value


def request_json(
    base_url: str,
    path: str,
    *,
    method: str = "GET",
    token: str | None = None,
    body: dict[str, object] | None = None,
) -> tuple[int, dict[str, object]]:
    data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
    request = urllib.request.Request(
        f"{base_url}{path}", data=data, method=method
    )
    request.add_header("Accept", "application/json")
    if data is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            raw = response.read(65536)
            return response.status, json.loads(raw or b"{}")
    except urllib.error.HTTPError as error:
        raw = error.read(65536)
        try:
            value = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            value = {}
        return error.code, value


def admin_login(base_url: str, username: str, password: str) -> str:
    status, value = request_json(
        base_url,
        "/auth/login",
        method="POST",
        body={
            "provider": "password",
            "client_id": ADMIN_CLIENT_ID,
            "scope": ADMIN_SCOPES,
            "credential": {"username": username, "password": password},
        },
    )
    token = str(value.get("access_token", ""))
    if status != 200 or not token:
        code = value.get("error", "missing_access_token")
        raise RuntimeError(f"Snaplink admin login failed: HTTP {status} ({code})")
    return token


def project_update(
    client_id: str,
    current: dict[str, object],
    desired: dict[str, object],
) -> dict[str, object]:
    projected = {
        field: current[field]
        for field in MUTABLE_CLIENT_FIELDS
        if field in current
    }
    projected["id"] = client_id
    projected.update(desired)
    return projected


def sync_client(
    base_url: str,
    token: str,
    client_id: str,
    desired: dict[str, object],
) -> bool:
    path = f"/api/v1/admin/clients/{urllib.parse.quote(client_id, safe='')}"
    status, value = request_json(base_url, path, token=token)
    current = value.get("client")
    if status != 200 or not isinstance(current, dict):
        code = value.get("error", "invalid_client_response")
        raise RuntimeError(f"get client {client_id} failed: HTTP {status} ({code})")

    if all(current.get(field) == value for field, value in desired.items()):
        return False

    update = project_update(client_id, current, desired)
    status, value = request_json(
        base_url, path, method="PUT", token=token, body=update
    )
    updated = value.get("client")
    if status != 200 or not isinstance(updated, dict):
        code = value.get("error", "invalid_client_response")
        raise RuntimeError(f"update client {client_id} failed: HTTP {status} ({code})")
    if not all(updated.get(field) == expected for field, expected in desired.items()):
        raise RuntimeError(f"client {client_id} verification failed")
    return True


def main() -> int:
    base_url = required("SNAPLINK_BASE_URL").rstrip("/")
    client_id = required("SNAPLINK_MANAGED_CLIENT_ID")
    desired = {
        "name": required("SNAPLINK_MANAGED_CLIENT_NAME"),
        "redirectUris": required("SNAPLINK_MANAGED_CLIENT_REDIRECT_URIS").split(),
        "loginPageUri": os.environ.get("SNAPLINK_MANAGED_CLIENT_LOGIN_PAGE_URI", "").strip(),
        "allowedScopes": required("SNAPLINK_MANAGED_CLIENT_SCOPES").split(),
        "allowedAuthenticators": required(
            "SNAPLINK_MANAGED_CLIENT_AUTHENTICATORS"
        ).split(),
        "tokenStrategy": required("SNAPLINK_MANAGED_CLIENT_TOKEN_STRATEGY"),
        "active": True,
    }
    token = admin_login(
        base_url,
        required("SNAPLINK_ADMIN_USERNAME"),
        required("SNAPLINK_ADMIN_PASSWORD"),
    )
    changed = sync_client(base_url, token, client_id, desired)
    state = "updated" if changed else "already current"
    print(f"Snaplink client {client_id}: {state}", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - one-shot controller boundary
        print(f"Snaplink config sync failed: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1)
