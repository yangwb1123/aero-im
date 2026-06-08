#!/usr/bin/env python3
"""Wave-20 smoke: reaction notifications — adding an emoji reaction to someone
else's message drops a durable 'reaction' notification in the author's inbox
(never self-notify; mute/DND/snooze still gate it).

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
            {"email": f"{tag}_w20+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W20"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def reaction_notifs(token, message_id):
    rows = as_list(req("GET", "/api/notifications", token=token), "notifications")
    return [n for n in rows if n.get("kind") == "reaction" and n.get("message_id") == message_id]


def main():
    ts = int(time.time())
    say("setup: alice + bob in a shared channel; alice posts a message")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave20 {ts}", "slug": f"w20-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "channel", "name": f"w20-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    M = req("POST", f"/api/rooms/{R}/command", {"text": "/me ships the release"}, token=A)["id"]
    ok(f"room {R[:8]} + message {M[:8]} by alice")

    # ---------------- Reaction notification ----------------
    say("reaction notification: bob reacts → alice gets a durable 'reaction' inbox entry")
    if reaction_notifs(A, M):
        fail("alice already has a reaction notification before any reaction")
    req("POST", f"/api/messages/{M}/reactions", {"emoji": "🚀"}, token=B, expect=[200])
    time.sleep(0.3)
    notifs = reaction_notifs(A, M)
    if not notifs:
        fail("alice did not receive a reaction notification after bob reacted")
    if notifs[0].get("actor_id") not in (Bpid, None):
        fail(f"reaction notification actor mismatch: {notifs[0]}")
    ok(f"alice received a reaction notification (actor=bob, kind=reaction)")

    # ---------------- No self-notify ----------------
    say("no self-notify: alice reacting to her OWN message creates no notification")
    before = len(reaction_notifs(A, M))
    req("POST", f"/api/messages/{M}/reactions", {"emoji": "👍"}, token=A, expect=[200])
    time.sleep(0.3)
    after = len(reaction_notifs(A, M))
    if after != before:
        fail(f"self-reaction created a notification (before={before}, after={after})")
    ok("alice's reaction to her own message produced no notification")

    # ---------------- Remove reaction is not a notification ----------------
    say("removing a reaction does not notify")
    before2 = len(reaction_notifs(A, M))
    req("POST", f"/api/messages/{M}/reactions", {"emoji": "🚀"}, token=B, expect=[200])  # toggles 🚀 off
    time.sleep(0.3)
    after2 = len(reaction_notifs(A, M))
    if after2 != before2:
        fail(f"removing a reaction created a notification (before={before2}, after={after2})")
    ok("removing a reaction produced no new notification")

    print("\n\033[1;32m✅ Wave-20 smoke PASSED (reaction notifications: notify-on-add, no self-notify, no notify-on-remove)\033[0m")


if __name__ == "__main__":
    main()
