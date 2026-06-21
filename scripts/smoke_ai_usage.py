#!/usr/bin/env python3
"""AI per-tenant usage endpoint smoke (ROADMAP 第六版 · 方向一·2).

Verifies the admin-gated GET /api/workspaces/:id/ai-usage surface: shape, the
workspace-admin gate (a non-member is rejected), and the `?since_secs` window. The
ledger is populated only by PAID AI charges (none in a no-key sandbox → empty
rollup), so this proves the read/gate surface end-to-end; the charge→drain→row
pipe is covered by the aero-ai sink unit test + the aero-storage db_test.

Run against a live server.
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    headers = {"accept": "application/json"}
    if token: headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect:
                fail(f"{method} {path}: expected {expect} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect:
                fail(f"{method} {path}: expected {expect} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def main():
    ts = int(time.time())
    say("register Alice (workspace owner = admin) + Bob (outsider)")
    a = req("POST", "/api/auth/register",
            {"email": f"a_usage+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceU"})
    b = req("POST", "/api/auth/register",
            {"email": f"b_usage+{ts}@aero.dev", "password": "password_1234", "display_name": "BobU"})
    A, B = a["access_token"], b["access_token"]
    ws = req("POST", "/api/workspaces", {"name": f"usage-{ts}", "slug": f"usage-{ts}"}, token=A)
    W = ws["id"]
    ok(f"workspace={W[:8]} (alice owner)")

    say("admin GET /ai-usage → 200 with the expected shape (empty ledger in a no-key sandbox)")
    u = req("GET", f"/api/workspaces/{W}/ai-usage", token=A, expect=200)
    for k in ("workspace_id", "since_secs", "total_micros", "by_kind"):
        if k not in u:
            fail(f"response missing '{k}': {u}")
    if u["workspace_id"] != W:
        fail(f"workspace_id mismatch: {u['workspace_id']}")
    if not isinstance(u["by_kind"], list):
        fail(f"by_kind not a list: {u}")
    if u["total_micros"] != 0 or u["by_kind"]:
        fail(f"fresh workspace should have zero usage: {u}")
    ok(f"shape ok; total_micros=0, by_kind=[] (default window {u['since_secs']}s)")

    say("?since_secs window is honored + clamped")
    u2 = req("GET", f"/api/workspaces/{W}/ai-usage?since_secs=3600", token=A, expect=200)
    if u2["since_secs"] != 3600:
        fail(f"since_secs not honored: {u2['since_secs']}")
    u3 = req("GET", f"/api/workspaces/{W}/ai-usage?since_secs=0", token=A, expect=200)
    if u3["since_secs"] < 1:
        fail(f"since_secs=0 should clamp to >=1: {u3['since_secs']}")
    ok(f"window honored (3600) and clamped (0→{u3['since_secs']})")

    say("non-member is REJECTED (workspace-admin gate)")
    req("GET", f"/api/workspaces/{W}/ai-usage", token=B, expect=403)
    ok("bob (non-member) → 403")

    say("unauthenticated → 401")
    req("GET", f"/api/workspaces/{W}/ai-usage", expect=401)
    ok("no token → 401")

    print("\n\033[1;32m✅ ai-usage endpoint smoke passed\033[0m")


if __name__ == "__main__":
    main()
