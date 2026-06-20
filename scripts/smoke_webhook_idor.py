#!/usr/bin/env python3
"""Webhook-revoke IDOR regression smoke.

The revoke handlers (DELETE /api/webhooks/{incoming,outgoing}/:id) carry only the
GLOBAL webhook id in the path and used to discard the caller identity — so ANY
authenticated user could revoke another room's (or tenant's) webhook by id,
silently breaking its integration. This proves the fix: a non-member is rejected
and the hook stays LIVE; the owning member can still revoke.

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
    say("register alice (room owner) + bob (outsider)")
    a = req("POST", "/api/auth/register",
            {"email": f"a_whidor+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceWh"})
    b = req("POST", "/api/auth/register",
            {"email": f"b_whidor+{ts}@aero.dev", "password": "password_1234", "display_name": "BobWh"})
    A, B = a["access_token"], b["access_token"]
    ok("registered")

    say("alice creates a private group room (bob is NOT a member)")
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"wh-{ts}"}, token=A)
    R = room["id"]
    ok(f"room={R[:8]}")

    say("alice registers an incoming + outgoing webhook")
    inc = req("POST", f"/api/rooms/{R}/webhooks/incoming", {"label": "ci"}, token=A, expect=200)
    out = req("POST", f"/api/rooms/{R}/webhooks/outgoing",
              {"url": "https://example.com/hook", "label": "deploys"}, token=A, expect=200)
    IN, OUT = inc["id"], out["id"]
    ok(f"incoming={IN[:8]} outgoing={OUT[:8]}")

    say("bob (non-member) is REJECTED revoking either hook by id (the IDOR fix)")
    req("DELETE", f"/api/webhooks/incoming/{IN}", token=B, expect=403)
    req("DELETE", f"/api/webhooks/outgoing/{OUT}", token=B, expect=403)
    ok("bob's cross-room revokes → 403")

    say("the hooks are still LIVE (bob's attempts changed nothing)")
    listed = req("GET", f"/api/rooms/{R}/webhooks", token=A, expect=200)
    inc_live = [h for h in listed["incoming"] if h["id"] == IN and not h["revoked"]]
    out_live = [h for h in listed["outgoing"] if h["id"] == OUT and not h["revoked"]]
    if not inc_live or not out_live:
        fail(f"a hook was revoked by the outsider: {listed}")
    ok("both hooks remain active")

    say("an unknown webhook id is 404 (no existence leak)")
    fake = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    req("DELETE", f"/api/webhooks/incoming/{fake}", token=A, expect=404)
    req("DELETE", f"/api/webhooks/outgoing/{fake}", token=A, expect=404)
    ok("unknown id → 404")

    say("the owning member CAN revoke (authorized path still works)")
    req("DELETE", f"/api/webhooks/incoming/{IN}", token=A, expect=204)
    req("DELETE", f"/api/webhooks/outgoing/{OUT}", token=A, expect=204)
    after = req("GET", f"/api/rooms/{R}/webhooks", token=A, expect=200)
    if any(h["id"] == IN and not h["revoked"] for h in after["incoming"]):
        fail("incoming hook not revoked by owner")
    if any(h["id"] == OUT and not h["revoked"] for h in after["outgoing"]):
        fail("outgoing hook not revoked by owner")
    ok("owner revoked both")

    print("\n\033[1;32m✅ webhook-revoke IDOR smoke passed\033[0m")


if __name__ == "__main__":
    main()
