#!/usr/bin/env python3
"""Wave-15 smoke: session management (token refresh + logout/revoke), advanced
search operators (from:/in:), AI smart replies, channel role management.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


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


def register(tag, ts):
    r = req("POST", "/api/auth/register",
            {"email": f"{tag}_w15+{ts}@aero.dev", "password": "password_1234", "display_name": f"{tag.capitalize()}W15"})
    return r["access_token"], r["refresh_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def send(room, text, token):
    m = req("POST", f"/api/rooms/{room}/command", {"text": f"/me {text}"}, token=token)
    return m.get("id") if isinstance(m, dict) else None


def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob (member)")
    A, A_refresh, Apid = register("alice", ts)
    B, B_refresh, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave15 {ts}", "slug": f"w15-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} with alice+bob")

    # ---------------- Session management ----------------
    say("session: refresh access token, then logout revokes the refresh token")
    rf = req("POST", "/api/auth/refresh", {"refresh_token": B_refresh})
    if not rf or not rf.get("access_token"):
        fail(f"refresh did not return a new access token: {rf}")
    new_access = rf["access_token"]
    req("GET", "/api/me", token=new_access, expect=[200])
    ok("refresh issued a working new access token")
    req("POST", "/api/auth/logout", {"refresh_token": B_refresh}, token=B, expect=[200, 204])
    req("POST", "/api/auth/refresh", {"refresh_token": B_refresh}, expect=[401])
    ok("logout revoked the refresh token (subsequent refresh → 401)")

    # ---------------- Advanced search operators ----------------
    say("search operators: from: / in: narrow the cross-room search")
    kw = f"zsearch{ts}"
    R1 = req("POST", "/api/rooms", {"kind": "group", "name": f"s1-{ts}", "workspace_id": W}, token=A)["id"]
    R2 = req("POST", "/api/rooms", {"kind": "group", "name": f"s2-{ts}", "workspace_id": W}, token=A)["id"]
    for R in (R1, R2):
        req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    send(R1, f"alice says {kw} in room one", A)
    send(R1, f"bob says {kw} in room one", B)
    send(R2, f"bob says {kw} in room two", B)
    time.sleep(0.4)
    # plain: all 3 hits
    allr = req("POST", "/api/search/advanced", {"query": kw, "workspace_id": W}, token=A)
    n_all = len(as_list(allr, "results"))
    # from:bob → only bob's 2
    fromb = req("POST", "/api/search/advanced", {"query": f"from:{Bpid} {kw}", "workspace_id": W}, token=A)
    n_from = len(as_list(fromb, "results"))
    # in:R2 → only room two (and from:bob there)
    inr2 = req("POST", "/api/search/advanced", {"query": f"in:{R2} {kw}", "workspace_id": W}, token=A)
    n_in = len(as_list(inr2, "results"))
    if not (n_all >= 3 and n_from == 2 and n_in == 1):
        fail(f"operator filtering off: all={n_all} from:bob={n_from} in:R2={n_in}")
    ok(f"operators filter correctly (all={n_all}, from:bob={n_from}, in:R2={n_in})")

    # ---------------- AI smart replies ----------------
    say("ai smart replies: suggest reply options for a room")
    sr = req("POST", f"/api/rooms/{R1}/suggest-replies", {}, token=A, expect=[200, 502])
    if sr is None:
        ok("suggest-replies reachable (502 — no LLM configured)")
    else:
        if "suggestions" not in sr:
            fail(f"missing suggestions: {sr}")
        ok(f"smart replies returned (len={len(str(sr.get('suggestions')))})")

    # ---------------- Channel role management ----------------
    say("channel roles: view roles, transfer ownership, owner-only mutation")
    Rc = req("POST", "/api/rooms", {"kind": "channel", "name": f"roles-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{Rc}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    roles = {r.get("participant_id"): r.get("role") for r in as_list(req("GET", f"/api/rooms/{Rc}/roles", token=A), "members")}
    if roles.get(Apid) != "owner":
        fail(f"alice not owner: {roles}")
    ok(f"roles visible (alice={roles.get(Apid)}, bob={roles.get(Bpid)})")
    # bob (non-owner) cannot mutate
    req("PUT", f"/api/rooms/{Rc}/roles/{Apid}", {"role": "member"}, token=B, expect=[403])
    # alice transfers ownership to bob
    req("POST", f"/api/rooms/{Rc}/transfer-ownership", {"to": Bpid}, token=A, expect=[200, 204])
    roles2 = {r.get("participant_id"): r.get("role") for r in as_list(req("GET", f"/api/rooms/{Rc}/roles", token=A), "members")}
    if roles2.get(Bpid) != "owner":
        fail(f"ownership not transferred to bob: {roles2}")
    ok(f"ownership transferred (bob={roles2.get(Bpid)}, alice={roles2.get(Apid)})")
    # alice (now non-owner) can no longer transfer
    req("POST", f"/api/rooms/{Rc}/transfer-ownership", {"to": Apid}, token=A, expect=[403])
    ok("former owner can no longer mutate roles (403)")

    print("\n\033[1;32m✅ Wave-15 smoke PASSED (session mgmt, search operators, AI smart replies, channel roles)\033[0m")


if __name__ == "__main__":
    main()
