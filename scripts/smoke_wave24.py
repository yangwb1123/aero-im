#!/usr/bin/env python3
"""Wave-24 smoke: workspace-wide 2FA enforcement. An admin mandates 2FA; a member
without activated TOTP is then locked out of that workspace's room data
(assert_room_access) until they enroll — while the /api/me/2fa enroll route stays
reachable. Non-admins cannot toggle the policy.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import base64, hashlib, hmac, json, os, struct, sys, time, urllib.error, urllib.request

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
                fail(f"{method} {path}: want {sorted(ok_codes)} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:200]}")


def totp_now(secret_b32, step=0):
    s = secret_b32.upper()
    s += "=" * ((8 - len(s) % 8) % 8)
    key = base64.b32decode(s)
    counter = int(time.time()) // 30 + step
    digest = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    off = digest[-1] & 0x0F
    code = (struct.unpack(">I", digest[off:off + 4])[0] & 0x7FFFFFFF) % 1_000_000
    return f"{code:06d}"


def register(tag, ts):
    r = req("POST", "/api/auth/register", {"email": f"{tag}_w24+{ts}@aero.dev", "password": "password_1234", "display_name": f"{tag.capitalize()}W24"})
    return r["access_token"], r["participant"]["id"]


def main():
    ts = int(time.time())
    say("setup: alice (owner) + bob (member); workspace + channel (both members)")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave24 {ts}", "slug": f"w24-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"w24-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} + channel {R[:8]}")

    say("before enforcement: bob can read room data")
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[200])
    ok("bob reads room messages (no mandate yet)")

    say("admin must activate 2FA before imposing the workspace mandate")
    req("PUT", f"/api/workspaces/{W}/security", {"require_2fa": True}, token=A, expect=[403])
    owner_enrollment = req("POST", "/api/me/2fa/enroll", token=A)
    owner_secret = owner_enrollment.get("secret")
    if not owner_secret:
        fail(f"owner enroll missing secret: {owner_enrollment}")
    req("POST", "/api/me/2fa/verify", {"code": totp_now(owner_secret)}, token=A, expect=[200])
    ok("unprotected owner was rejected; owner activated TOTP")

    say("protected admin mandates 2FA → bob (no TOTP) is locked out of room data")
    sec = req("PUT", f"/api/workspaces/{W}/security", {"require_2fa": True}, token=A, expect=[200])
    if sec.get("require_2fa") is not True:
        fail(f"set_require_2fa did not stick: {sec}")
    # non-admin cannot toggle the policy
    req("PUT", f"/api/workspaces/{W}/security", {"require_2fa": False}, token=B, expect=[403])
    # bob, a member without activated TOTP, is now denied room data
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[403])
    ok("require_2fa set (admin-only); bob's room access → 403 (2fa_required)")

    say("enrollment stays reachable; after verify, bob regains room access")
    enr = req("POST", "/api/me/2fa/enroll", token=B)
    secret = enr.get("secret")
    if not secret:
        fail(f"enroll missing secret (enroll must NOT be room-gated): {enr}")
    req("POST", "/api/me/2fa/verify", {"code": totp_now(secret)}, token=B, expect=[200])
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[200])
    ok("bob enrolled+verified TOTP → room access restored (200)")

    say("policy is readable by members; admin can lift it")
    g = req("GET", f"/api/workspaces/{W}/security", token=B)
    if g.get("require_2fa") is not True:
        fail(f"member cannot read policy or wrong value: {g}")
    req("PUT", f"/api/workspaces/{W}/security", {"require_2fa": False}, token=A, expect=[200])
    ok("members read the policy; admin lifted the mandate")

    print("\n\033[1;32m✅ Wave-24 smoke PASSED (workspace 2FA enforcement: gate on room data, enroll-to-unlock, admin-only toggle)\033[0m")


if __name__ == "__main__":
    main()
