#!/usr/bin/env python3
"""Wave-21 smoke: active session inventory + remote/global sign-out, and the
activity feed (CRUD; go-live fan-out is covered by the PG db_test + golive_bot,
which needs a real WHIP push to drive end-to-end).

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import http.client, json, os, sys, time, urllib.error, urllib.parse, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
_login_stamp = time.time_ns()
LOGIN_SOURCE_IP = (
    f"127.{(_login_stamp >> 16) % 250 + 1}."
    f"{(_login_stamp >> 8) % 250 + 1}.{_login_stamp % 250 + 1}"
)


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    ok_codes = {expect} if isinstance(expect, int) else (set(expect) if expect else None)
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if ok_codes is not None and resp.status not in ok_codes:
                fail(f"{method} {path}: want {sorted(ok_codes)} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if ok_codes is not None:
            if e.code not in ok_codes:
                fail(f"{method} {path}: want {sorted(ok_codes)} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


EMAIL = None
PASSWORD = "password_1234"


def register(tag, ts):
    global EMAIL
    EMAIL = f"{tag}_w21+{ts}@aero.dev"
    r = req("POST", "/api/auth/register", {"email": EMAIL, "password": PASSWORD, "display_name": f"{tag.capitalize()}W21"})
    return r["access_token"], r["refresh_token"], r["participant"]["id"]


def login():
    parsed = urllib.parse.urlsplit(HOST)
    connection_type = (
        http.client.HTTPSConnection
        if parsed.scheme == "https"
        else http.client.HTTPConnection
    )
    connection = connection_type(
        parsed.hostname,
        parsed.port,
        timeout=10,
        source_address=(LOGIN_SOURCE_IP, 0),
    )
    connection.request(
        "POST",
        f"{parsed.path.rstrip('/')}/api/auth/login",
        body=json.dumps({"email": EMAIL, "password": PASSWORD}).encode(),
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    connection.close()
    if status != 200:
        fail(f"login from dedicated source failed: {status} {raw.decode(errors='ignore')[:200]}")
    r = json.loads(raw)
    return r["access_token"], r["refresh_token"]


def sessions(token):
    v = req("GET", "/api/auth/sessions", token=token)
    return v if isinstance(v, list) else v.get("sessions", [])


def main():
    ts = int(time.time())
    say("setup: alice registers (device 1), then logs in twice more (devices 2 + 3)")
    # jti nonce (UUID v4) in every refresh token guarantees distinct hashes even
    # when all three logins happen in the same second — no sleep needed.
    a1_access, a1_refresh, Apid = register("alice", ts)
    a2_access, a2_refresh = login()
    a3_access, a3_refresh = login()
    sl = sessions(a3_access)
    if len(sl) != 3:
        fail(f"expected 3 active sessions, got {len(sl)}: {sl}")
    ok(f"3 active sessions tracked (one per login)")

    # ---------------- Remote / global sign-out ----------------
    say("sign out everywhere else: revoke all sessions except the current one")
    rev = req("POST", "/api/auth/sessions/revoke-others", {"current_refresh_token": a3_refresh}, token=a3_access)
    if rev.get("revoked_count") != 2:
        fail(f"expected to revoke 2 other sessions, got {rev}")
    if len(sessions(a3_access)) != 1:
        fail("expected exactly 1 session left after revoke-others")
    # the two revoked devices' refresh tokens no longer work; the current one does
    req("POST", "/api/auth/refresh", {"refresh_token": a1_refresh}, expect=[401])
    req("POST", "/api/auth/refresh", {"refresh_token": a2_refresh}, expect=[401])
    r3 = req("POST", "/api/auth/refresh", {"refresh_token": a3_refresh}, expect=[200])
    if not r3 or not r3.get("access_token"):
        fail(f"current device's refresh should still work: {r3}")
    ok("revoke-others killed the 2 other devices (refresh→401); current device still refreshes")

    # ---------------- Revoke a single session ----------------
    say("revoke one session by id")
    cur = r3["access_token"]
    sl2 = sessions(cur)
    if not sl2:
        fail("no sessions to revoke")
    sid = sl2[0]["id"]
    d = req("DELETE", f"/api/auth/sessions/{sid}", token=cur, expect=[200])
    if not d.get("revoked"):
        fail(f"single revoke did not report success: {d}")
    req("GET", "/api/me", token=cur, expect=[401])
    activity_access, _activity_refresh = login()
    ok("revoked a single session by id; its access token stopped working")

    # ---------------- Activity feed (CRUD) ----------------
    say("activity feed: list / unread count / mark read (go-live fan-out is db-tested)")
    feed = req("GET", "/api/activity", token=activity_access)
    if not isinstance(feed if isinstance(feed, list) else feed.get("entries", feed.get("activity")), list):
        fail(f"activity feed not a list: {feed}")
    cnt = req("GET", "/api/activity/count", token=activity_access)
    if "unread" not in cnt:
        fail(f"activity count missing 'unread': {cnt}")
    mk = req("POST", "/api/activity/read", token=activity_access, expect=[200])
    if "marked" not in mk:
        fail(f"activity read missing 'marked': {mk}")
    ok(f"activity feed reachable (unread={cnt.get('unread')}, list ok, mark-read ok)")

    print("\n\033[1;32m✅ Wave-21 smoke PASSED (active sessions + remote sign-out; activity feed CRUD)\033[0m")


if __name__ == "__main__":
    main()
