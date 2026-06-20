#!/usr/bin/env python3
"""IP-allowlist *enforcement* smoke — proves the middleware actually 403s.

The storage matcher (`is_allowed`) + admin-management routes shipped earlier, but
nothing in any request path called them: a workspace could declare authorized
networks that were silently never enforced. This exercises the
`ip_allowlist::enforce_layer` middleware end-to-end:

  * empty allowlist            ⇒ allow-all (200)
  * IP inside  a non-empty list⇒ 200
  * IP outside a non-empty list⇒ 403
  * the `/ip-allowlist` mgmt route is NEVER enforced (anti-lockout), so an admin
    who fat-fingers a range that excludes their own IP can still fix it.

`client_ip` trusts `X-Forwarded-For` ahead of the socket peer, so we drive the
allow/deny paths deterministically by spoofing XFF (the real peer is 127.0.0.1).
Run against a live server.
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, xff=None, expect=None):
    headers = {"accept": "application/json"}
    if token: headers["authorization"] = f"Bearer {token}"
    if xff: headers["x-forwarded-for"] = xff
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect:
                fail(f"{method} {path} (xff={xff}): expected {expect} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect:
                fail(f"{method} {path} (xff={xff}): expected {expect} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def main():
    ts = int(time.time())
    say("register Alice (owner of a fresh workspace)")
    a = req("POST", "/api/auth/register",
            {"email": f"a_ipal+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceIpal"})
    A = a["access_token"]
    ws = req("POST", "/api/workspaces", {"name": f"ipal-{ts}", "slug": f"ipal-{ts}"}, token=A)
    W = ws["id"]
    ok(f"workspace={W[:8]} created")

    chans = f"/api/workspaces/{W}/channels"
    allowl = f"/api/workspaces/{W}/ip-allowlist"

    say("empty allowlist ⇒ allow-all")
    req("GET", chans, token=A, expect=200)
    ok("non-workspace-admin route reachable with no allowlist")

    say("admin adds an authorized network that EXCLUDES typical clients")
    req("POST", allowl, {"cidr": "203.0.113.0/24", "note": "office"}, token=A, expect=200)
    listed = req("GET", allowl, token=A, expect=200)
    if not listed or not listed.get("enabled"):
        fail(f"allowlist not reported enabled: {listed}")
    ok("203.0.113.0/24 added, allowlist reports enabled")

    say("a client INSIDE the allowlist passes (200)")
    req("GET", chans, token=A, xff="203.0.113.5", expect=200)
    ok("XFF 203.0.113.5 (in-range) ⇒ 200")

    say("a client OUTSIDE the allowlist is rejected (403)")
    req("GET", chans, token=A, xff="8.8.8.8", expect=403)
    ok("XFF 8.8.8.8 (out-of-range) ⇒ 403")
    # The real socket peer (127.0.0.1) is also outside 203.0.113.0/24:
    req("GET", chans, token=A, expect=403)
    ok("no-XFF localhost peer (out-of-range) ⇒ 403")

    say("anti-lockout: the mgmt route is NEVER enforced, even from a blocked IP")
    req("GET", allowl, token=A, xff="8.8.8.8", expect=200)
    ok("GET /ip-allowlist from blocked IP ⇒ 200 (admin not locked out)")
    req("DELETE", allowl, {"cidr": "203.0.113.0/24"}, token=A, xff="8.8.8.8", expect=204)
    ok("DELETE /ip-allowlist from blocked IP ⇒ 204 (can repair from anywhere)")

    say("allowlist emptied ⇒ allow-all restored")
    req("GET", chans, token=A, xff="8.8.8.8", expect=200)
    ok("previously-blocked IP ⇒ 200 again")

    say("a list that INCLUDES the client allows it")
    req("POST", allowl, {"cidr": "8.8.8.0/24"}, token=A, expect=200)
    req("GET", chans, token=A, xff="8.8.8.8", expect=200)
    ok("XFF 8.8.8.8 with 8.8.8.0/24 listed ⇒ 200")
    req("GET", chans, token=A, xff="9.9.9.9", expect=403)
    ok("XFF 9.9.9.9 (still out-of-range) ⇒ 403")

    print("\n\033[1;32m✅ ip-allowlist enforcement smoke passed\033[0m")


if __name__ == "__main__":
    main()
