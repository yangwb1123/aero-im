#!/usr/bin/env python3
"""Wave-14 smoke: 2FA/TOTP (enroll → verify → login enforcement), workspace user
deactivation (access revocation), message templates.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import base64, hashlib, hmac, http.client, json, os, struct, sys, time, urllib.error, urllib.parse, urllib.request

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


def login_req(body, expect):
    """Use a private loopback source so this scenario owns one fresh 5/min bucket."""
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
    path = f"{parsed.path.rstrip('/')}/api/auth/login"
    connection.request(
        "POST",
        path,
        body=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Accept": "application/json"},
    )
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    connection.close()
    if status not in set(expect):
        fail(f"POST /api/auth/login: want {sorted(expect)} got {status}: "
             f"{raw.decode(errors='ignore')[:200]}")
    return json.loads(raw) if raw else None


def register(tag, ts):
    email = f"{tag}_w14+{ts}@aero.dev"
    r = req("POST", "/api/auth/register",
            {"email": email, "password": "password_1234", "display_name": f"{tag.capitalize()}W14"})
    return r["access_token"], r["participant"]["id"], email


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def totp_now(secret_b32, step=0):
    """RFC 6238 TOTP, SHA1, 6 digits, 30s period (+step offset)."""
    # pad base32 to a multiple of 8 for python's strict decoder
    s = secret_b32.upper()
    s += "=" * ((8 - len(s) % 8) % 8)
    key = base64.b32decode(s)
    counter = int(time.time()) // 30 + step
    msg = struct.pack(">Q", counter)
    digest = hmac.new(key, msg, hashlib.sha1).digest()
    off = digest[-1] & 0x0F
    code = (struct.unpack(">I", digest[off:off + 4])[0] & 0x7FFFFFFF) % 1_000_000
    return f"{code:06d}"


def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob (member)")
    A, Apid, _A_email = register("alice", ts)
    B, Bpid, B_email = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave14 {ts}", "slug": f"w14-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} with alice+bob")

    # ---------------- 2FA / TOTP ----------------
    say("2FA: enroll → verify → recovery-code login → TOTP login → disable")
    st0 = req("GET", "/api/me/2fa", token=B)
    if st0.get("activated"):
        fail(f"bob already 2fa-activated: {st0}")
    enr = req("POST", "/api/me/2fa/enroll", token=B)
    secret = enr.get("secret")
    if not secret or "otpauth://" not in str(enr.get("otpauth_uri", "")):
        fail(f"enroll missing secret/uri: {enr}")
    ok("enrolled (secret + otpauth uri issued)")
    # wrong code rejected
    req("POST", "/api/me/2fa/verify", {"code": "000000"}, token=B, expect=[400])
    # correct code activates
    req("POST", "/api/me/2fa/verify", {"code": totp_now(secret)}, token=B, expect=[200])
    st1 = req("GET", "/api/me/2fa", token=B)
    if not st1.get("activated"):
        fail(f"2fa not activated after verify: {st1}")
    ok("verified with a live TOTP code → activated")
    # login WITHOUT totp is now rejected
    login_req({"email": B_email, "password": "password_1234"}, [401, 403])
    recovery = req("POST", "/api/me/2fa/recovery-codes",
                   {"code": totp_now(secret)}, token=B)
    codes = recovery.get("codes", []) if isinstance(recovery, dict) else []
    if len(codes) != 8 or any(len(code) != 16 for code in codes):
        fail(f"recovery-code batch has wrong shape: {recovery}")
    code = codes[0]
    # Password is always checked before the recovery code, so a wrong password
    # cannot burn a valid one-time code.
    login_req(
        {"email": B_email, "password": "wrong-password", "recovery_code": code},
        [401, 403],
    )
    recovered = login_req(
        {"email": B_email, "password": "password_1234", "recovery_code": code},
        [200],
    )
    if not recovered or not recovered.get("access_token"):
        fail(f"login with recovery code failed: {recovered}")
    login_req(
        {"email": B_email, "password": "password_1234", "recovery_code": code},
        [401, 403],
    )
    ok("recovery code: password-first, one-time, wrong/replay rejected")
    # login WITH a valid totp succeeds
    lg = login_req(
        {"email": B_email, "password": "password_1234", "totp": totp_now(secret)},
        [200],
    )
    if not lg or not lg.get("access_token"):
        fail(f"login with totp failed: {lg}")
    ok("login enforcement: no code → 401; valid TOTP → 200")
    # disable (requires a valid code)
    req("DELETE", "/api/me/2fa", {"code": "000000"}, token=B, expect=[400])
    req("DELETE", "/api/me/2fa", {"code": totp_now(secret)}, token=B, expect=[200, 204])
    st2 = req("GET", "/api/me/2fa", token=B)
    if st2.get("activated"):
        fail(f"2fa remained active after disable: {st2}")
    ok("disabled (bad code 400; valid code disables) → status inactive")

    # ---------------- User deactivation ----------------
    say("deactivation: revoke a member's workspace room access")
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"deact-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[200])
    ok("bob can access the room before deactivation")
    req("POST", f"/api/workspaces/{W}/members/{Apid}/deactivate", token=A, expect=[400])  # self
    req("POST", f"/api/workspaces/{W}/members/{Apid}/deactivate", token=B, expect=[403])  # non-admin
    req("POST", f"/api/workspaces/{W}/members/{Bpid}/deactivate", token=A, expect=[200, 204])
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[403])
    deact = as_list(req("GET", f"/api/workspaces/{W}/deactivated", token=A), "members")
    if not any((m.get("participant_id") == Bpid) for m in deact):
        fail(f"bob not in deactivated list: {deact}")
    ok("deactivated bob → room access 403; listed; self/non-admin guarded")
    req("POST", f"/api/workspaces/{W}/members/{Bpid}/reactivate", token=A, expect=[200, 204])
    req("GET", f"/api/rooms/{R}/messages", token=B, expect=[200])
    ok("reactivated bob → room access restored")

    # ---------------- Message templates ----------------
    say("templates: create, list, send into a room, delete (owner-scoped)")
    tmpl = req("POST", "/api/templates",
               {"name": "standup", "blocks": [{"type": "text", "content": f"daily standup {ts}"}]}, token=A)
    Tid = tmpl.get("id") or (tmpl.get("template") or {}).get("id")
    if not Tid:
        fail(f"no template id: {tmpl}")
    tl = as_list(req("GET", "/api/templates", token=A), "templates")
    if not any(t.get("id") == Tid for t in tl):
        fail(f"template not listed: {tl}")
    ok(f"template created + listed ({len(tl)})")
    sent = req("POST", f"/api/templates/{Tid}/send", {"room_id": R}, token=A)
    if not (isinstance(sent, dict) and sent.get("id")):
        fail(f"template send did not return a message: {sent}")
    ok("template sent into a room (message created)")
    req("DELETE", f"/api/templates/{Tid}", token=B, expect=[403, 404])
    req("DELETE", f"/api/templates/{Tid}", token=A, expect=[200, 204])
    ok("owner-scoped delete (stranger blocked; owner deletes)")

    print("\n\033[1;32m✅ Wave-14 smoke PASSED (2FA/TOTP/recovery, user deactivation, message templates)\033[0m")


if __name__ == "__main__":
    main()
