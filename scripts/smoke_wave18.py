#!/usr/bin/env python3
"""Wave-18 smoke: call history, stream key rotation, AI writing assistant
(rewrite), live-stream clips, stream creator analytics, mark-as-unread.

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
            {"email": f"{tag}_w18+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W18"})
    return r["access_token"], r["participant"]["id"]


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


def unread_for(room, token):
    rows = as_list(req("GET", "/api/unread", token=token))
    for r in rows:
        if r.get("room_id") == room:
            return r.get("unread", 0)
    return 0


def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob; workspace + channel + stream")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave18 {ts}", "slug": f"w18-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"w18-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    S = req("POST", "/api/streams", {"title": "w18 stream", "protocol": "rtmp", "room_id": R}, token=A)
    Sid = S["id"]
    ok(f"workspace {W[:8]} + channel {R[:8]} + stream {Sid[:8]}")

    # ---------------- Call history ----------------
    say("call history: read-only call-log endpoint (room-gated)")
    ch = req("GET", f"/api/rooms/{R}/calls", token=A, expect=[200])
    if not isinstance(as_list(ch, "calls"), list):
        fail(f"calls not a list: {ch}")
    # non-member dave cannot read the call log
    D, _ = register("dave", ts)
    req("GET", f"/api/rooms/{R}/calls", token=D, expect=[403, 404])
    ok("call history endpoint reachable for member; non-member 403")

    # ---------------- Stream key rotation ----------------
    say("stream key rotation: owner rotates; key changes; non-owner 403")
    old_key = S.get("stream_key")
    rot = req("POST", f"/api/streams/{Sid}/rotate-key", token=A, expect=[200])
    new_key = rot.get("stream_key")
    if not new_key:
        fail(f"rotate-key returned no key: {rot}")
    if old_key and new_key == old_key:
        fail("stream key did not change after rotation")
    req("POST", f"/api/streams/{Sid}/rotate-key", token=B, expect=[403, 404])
    ok(f"key rotated ({'changed' if old_key else 'new key issued'}); non-owner 403")

    # ---------------- AI writing assistant ----------------
    say("ai rewrite: rewrite a draft (degrades to echo without LLM key); empty 400")
    rw = req("POST", "/api/ai/rewrite", {"text": "hey can u send me teh report", "style": "professional"},
             token=A, expect=[200, 502])
    if rw is not None:
        if "rewritten" not in rw:
            fail(f"rewrite missing 'rewritten': {rw}")
        ok(f"rewrite returned (len={len(str(rw.get('rewritten')))})")
    else:
        ok("rewrite reachable (502 — no AI backend)")
    req("POST", "/api/ai/rewrite", {"text": "   ", "style": "concise"}, token=A, expect=[400])
    ok("empty text rejected (400)")

    # ---------------- Live-stream clips ----------------
    say("clips: create [start,end] range, list, get, delete; bad range 400")
    clip = req("POST", f"/api/streams/{Sid}/clips", {"title": "epic moment", "start_secs": 30, "end_secs": 55}, token=B)
    cid = clip["id"]
    clips = as_list(req("GET", f"/api/streams/{Sid}/clips", token=A), "clips")
    if not any(c.get("id") == cid for c in clips):
        fail(f"clip not listed: {clips}")
    got = req("GET", f"/api/clips/{cid}", token=A)
    if got.get("id") != cid:
        fail(f"clip get mismatch: {got}")
    # invalid range rejected
    req("POST", f"/api/streams/{Sid}/clips", {"title": "bad", "start_secs": 50, "end_secs": 10}, token=B, expect=[400])
    # creator deletes own clip
    req("DELETE", f"/api/clips/{cid}", token=B, expect=[200])
    ok("clip create/list/get/delete ok; bad range 400")

    # ---------------- Stream creator analytics ----------------
    say("stream analytics: owner-only aggregate dashboard")
    an = req("GET", f"/api/streams/{Sid}/analytics", token=A, expect=[200])
    for k in ("gift_count", "chat_count"):
        if k not in an:
            fail(f"analytics missing {k}: {an}")
    req("GET", f"/api/streams/{Sid}/analytics", token=B, expect=[403, 404])
    ok(f"analytics ok (gifts={an.get('gift_count')}, chat={an.get('chat_count')}); non-owner 403")

    # ---------------- Mark-as-unread ----------------
    say("mark-as-unread: read all, then roll the cursor back so the room re-badges")
    m1 = send(R, "first", A)
    m2 = send(R, "second", A)
    m3 = send(R, "third", A)
    time.sleep(0.3)
    req("POST", f"/api/rooms/{R}/read", {"last_message_id": m3}, token=B, expect=[200])
    if unread_for(R, B) != 0:
        fail(f"room should be fully read, unread={unread_for(R, B)}")
    req("POST", f"/api/messages/{m2}/mark-unread", token=B, expect=[200])
    time.sleep(0.2)
    after = unread_for(R, B)
    if after < 1:
        fail(f"mark-unread did not re-badge the room (unread={after})")
    ok(f"read→unread=0, then mark-unread m2 → unread={after} (re-badged)")

    print("\n\033[1;32m✅ Wave-18 smoke PASSED "
          "(call history, stream key rotation, AI rewrite, clips, stream analytics, mark-as-unread)\033[0m")


if __name__ == "__main__":
    main()
