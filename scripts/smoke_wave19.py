#!/usr/bin/env python3
"""Wave-19 smoke: per-channel retention override, information barriers (admin
CRUD; barred() logic is db-tested), snooze notifications, workspace-wide RAG ask.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request
from datetime import datetime, timedelta, timezone

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
            {"email": f"{tag}_w19+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W19"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def main():
    ts = int(time.time())
    say("setup: alice (owner) + bob + carol; workspace + channel")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    C, Cpid = register("carol", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave19 {ts}", "slug": f"w19-{ts}"}, token=A)["id"]
    for pid in (Bpid, Cpid):
        req("POST", f"/api/workspaces/{W}/members", {"participant_id": pid, "role": "member"}, token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"w19-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} + channel {R[:8]}")

    # ---------------- Per-channel retention override ----------------
    say("retention: set room override, read effective=COALESCE(room,workspace), clear, gate")
    req("PUT", f"/api/rooms/{R}/retention", {"days": 7}, token=A, expect=[200])
    v = req("GET", f"/api/rooms/{R}/retention", token=A)
    if v.get("effective") != 7 or v.get("room") != 7:
        fail(f"retention override not applied: {v}")
    req("PUT", f"/api/rooms/{R}/retention", {"days": None}, token=A, expect=[200])
    v2 = req("GET", f"/api/rooms/{R}/retention", token=A)
    if v2.get("room") is not None:
        fail(f"retention override not cleared: {v2}")
    # out-of-range rejected; non-creator/non-admin member rejected
    req("PUT", f"/api/rooms/{R}/retention", {"days": 99999}, token=A, expect=[400])
    req("PUT", f"/api/rooms/{R}/retention", {"days": 30}, token=B, expect=[403])
    ok(f"override set(7)→effective 7, cleared→inherit; out-of-range 400; non-admin 403")

    # ---------------- Information barriers (admin CRUD) ----------------
    say("info barriers: admin defines a barred user-group pair; list; delete")
    g1 = req("POST", f"/api/workspaces/{W}/user-groups", {"handle": f"traders{ts}", "name": "Traders"}, token=A)["id"]
    g2 = req("POST", f"/api/workspaces/{W}/user-groups", {"handle": f"research{ts}", "name": "Research"}, token=A)["id"]
    bar = req("POST", f"/api/workspaces/{W}/barriers", {"group_a": g1, "group_b": g2}, token=A)
    bid = bar["id"]
    bars = as_list(req("GET", f"/api/workspaces/{W}/barriers", token=A), "barriers")
    if not any(b.get("id") == bid for b in bars):
        fail(f"barrier not listed: {bars}")
    # non-admin member cannot create a barrier
    req("POST", f"/api/workspaces/{W}/barriers", {"group_a": g1, "group_b": g2}, token=B, expect=[403])
    req("DELETE", f"/api/barriers/{bid}", token=A, expect=[200])
    bars2 = as_list(req("GET", f"/api/workspaces/{W}/barriers", token=A), "barriers")
    if any(b.get("id") == bid for b in bars2):
        fail(f"barrier not deleted: {bars2}")
    ok("barrier create/list/delete ok; non-admin 403 (DM-enforcement covered by PG db_test)")

    # ---------------- Snooze notifications ----------------
    say("snooze: pause until a future time; reject past; clear")
    until = (datetime.now(timezone.utc) + timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
    req("PUT", "/api/notifications/snooze", {"until": until}, token=B, expect=[200])
    snz = req("GET", "/api/notifications/snooze", token=B)
    if not snz.get("snooze_until"):
        fail(f"snooze not set: {snz}")
    past = (datetime.now(timezone.utc) - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
    req("PUT", "/api/notifications/snooze", {"until": past}, token=B, expect=[400])
    req("DELETE", "/api/notifications/snooze", token=B, expect=[200])
    snz2 = req("GET", "/api/notifications/snooze", token=B)
    if snz2.get("snooze_until"):
        fail(f"snooze not cleared: {snz2}")
    ok("snooze set (future) / reject past 400 / cleared")

    # ---------------- Workspace-wide RAG ask ----------------
    say("workspace ask: cross-channel Q&A (membership-bounded); non-member 403")
    for t in ("the deploy window is friday", "the on-call is alice"):
        req("POST", f"/api/rooms/{R}/command", {"text": f"/me {t}"}, token=A)
    time.sleep(0.3)
    ans = req("POST", f"/api/workspaces/{W}/ask", {"question": "when is the deploy window?"}, token=A, expect=[200, 502])
    if ans is not None:
        if "answer" not in ans:
            fail(f"workspace ask missing 'answer': {ans}")
        ok(f"workspace ask answered (len={len(str(ans.get('answer')))})")
    else:
        ok("workspace ask reachable (502 — no AI backend)")
    D, _ = register("dave", ts)  # not a member of W
    req("POST", f"/api/workspaces/{W}/ask", {"question": "secret?"}, token=D, expect=[403])
    ok("non-member cannot ask the workspace (403)")

    print("\n\033[1;32m✅ Wave-19 smoke PASSED "
          "(per-channel retention, information barriers, snooze, workspace RAG ask)\033[0m")


if __name__ == "__main__":
    main()
