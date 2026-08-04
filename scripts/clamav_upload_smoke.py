#!/usr/bin/env python3
"""Verify Aero's real room-upload -> clamd INSTREAM enforcement path.

Run this against a gateway started with ``AERO_CLAMAV_HOST``.  The script
creates an isolated user and room, proves a clean upload is persisted, then
proves ClamAV's standard EICAR test file is rejected before blob storage.
Credentials stay in memory and are never printed.
"""

from __future__ import annotations

import argparse
import json
import secrets
import sys
import time
import urllib.error
import urllib.request
from typing import Any


class SmokeFailure(RuntimeError):
    """The configured upload path did not enforce the expected AV policy."""


def request_json(
    base_url: str,
    method: str,
    path: str,
    *,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    accepted: tuple[int, ...] = (200,),
) -> tuple[int, Any]:
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    payload = None
    if body is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(body, separators=(",", ":")).encode()
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}{path}",
        method=method,
        data=payload,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            status, raw = response.status, response.read(1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read(1024 * 1024 + 1)
    except OSError as error:
        raise SmokeFailure(f"{method} {path} failed: {type(error).__name__}") from error
    if status not in accepted:
        raise SmokeFailure(
            f"{method} {path} returned HTTP {status}; expected {accepted}"
        )
    if len(raw) > 1024 * 1024:
        raise SmokeFailure(f"{method} {path} response exceeded 1 MiB")
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise SmokeFailure(f"{method} {path} returned invalid JSON") from error


def multipart(filename: str, payload: bytes) -> tuple[str, bytes]:
    boundary = f"----aero-av-{secrets.token_hex(12)}"
    head = (
        f"--{boundary}\r\n"
        f'content-disposition: form-data; name="file"; filename="{filename}"\r\n'
        "content-type: text/plain\r\n\r\n"
    ).encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    return boundary, head + payload + tail


def upload(
    base_url: str,
    room_id: str,
    token: str,
    filename: str,
    payload: bytes,
    accepted: tuple[int, ...],
) -> tuple[int, Any]:
    boundary, body = multipart(filename, payload)
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}/api/rooms/{room_id}/blobs",
        method="POST",
        data=body,
        headers={
            "accept": "application/json",
            "authorization": f"Bearer {token}",
            "content-type": f"multipart/form-data; boundary={boundary}",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            status, raw = response.status, response.read(1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read(1024 * 1024 + 1)
    except OSError as error:
        raise SmokeFailure(f"upload failed: {type(error).__name__}") from error
    if status not in accepted:
        raise SmokeFailure(f"upload returned HTTP {status}; expected {accepted}")
    try:
        document = json.loads(raw) if raw else None
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise SmokeFailure("upload returned invalid JSON") from error
    return status, document


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:3030",
        help="running Aero gateway origin",
    )
    args = parser.parse_args()
    suffix = f"{time.time_ns():x}-{secrets.token_hex(5)}"
    password = secrets.token_urlsafe(32)
    _, registered = request_json(
        args.base_url,
        "POST",
        "/api/auth/register",
        body={
            "email": f"clamav-smoke+{suffix}@aero.test",
            "password": password,
            "display_name": "ClamAV Smoke",
        },
    )
    token = str(registered["access_token"])
    _, room = request_json(
        args.base_url,
        "POST",
        "/api/rooms",
        token=token,
        body={"kind": "group", "name": f"clamav-smoke-{suffix}"},
    )
    room_id = str(room["id"])

    clean_status, clean = upload(
        args.base_url,
        room_id,
        token,
        "clean.txt",
        b"Aero ClamAV clean staging payload.\n",
        (200,),
    )
    if not isinstance(clean, dict) or not clean.get("id"):
        raise SmokeFailure("clean upload did not return a blob id")

    # Official EICAR anti-malware test payload (not executable malware).
    eicar = (
        b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$"
        b"EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*"
    )
    infected_status, infected = upload(
        args.base_url,
        room_id,
        token,
        "eicar.txt",
        eicar,
        (400,),
    )
    diagnostic = json.dumps(infected, ensure_ascii=False).lower()
    if "virus scan" not in diagnostic and "eicar" not in diagnostic:
        raise SmokeFailure("infected upload rejection did not identify AV policy")

    print(
        json.dumps(
            {
                "ok": True,
                "clean_upload_status": clean_status,
                "infected_upload_status": infected_status,
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SmokeFailure as error:
        print(f"ClamAV upload smoke failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
