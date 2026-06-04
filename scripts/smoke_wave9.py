#!/usr/bin/env python3
"""Wave-9 smoke: channel sections (sidebar folders), saved searches, and
message-anchored reminders ("remind me about this message").

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
            {"email": f"{tag}_w9+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W9"})
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
    say("setup: register alice/bob; alice creates a workspace she owns")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    ws = req("POST", "/api/workspaces", {"name": f"Wave9 {ts}", "slug": f"w9-{ts}"}, token=A)
    W = ws["id"]
    # Bob is a workspace member (so the per-user privacy check below exercises
    # "member-but-not-owner sees none", not the workspace-membership gate).
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"},
        token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} owned by alice; bob enrolled as member")

    # ---------------- Channel sections ----------------
    say("sections: create, assign channels, list, remove, rename, delete; private per user")
    r1 = req("POST", "/api/rooms", {"kind": "channel", "name": f"chan-a-{ts}", "workspace_id": W}, token=A)["id"]
    r2 = req("POST", "/api/rooms", {"kind": "channel", "name": f"chan-b-{ts}", "workspace_id": W}, token=A)["id"]
    sec = req("POST", f"/api/workspaces/{W}/sections", {"name": "Favorites"}, token=A)
    Sid = sec.get("id") or sec.get("section", {}).get("id")
    if not Sid:
        fail(f"no section id: {sec}")
    req("PUT", f"/api/sections/{Sid}/channels/{r1}", token=A, expect=[200, 204])
    req("PUT", f"/api/sections/{Sid}/channels/{r2}", token=A, expect=[200, 204])
    secs = as_list(req("GET", f"/api/workspaces/{W}/sections", token=A), "sections")
    mine = next((s for s in secs if s.get("id") == Sid), None)
    if not mine:
        fail(f"section not listed: {secs}")
    rooms_in = as_list(mine, "room_ids", "rooms", "channels") or mine.get("room_ids", [])
    if not ({r1, r2} <= set(rooms_in)):
        fail(f"channels not in section: {mine}")
    ok(f"section created with 2 channels ({len(rooms_in)} listed)")
    req("DELETE", f"/api/sections/{Sid}/channels/{r1}", token=A, expect=[200, 204])
    req("PATCH", f"/api/sections/{Sid}", {"name": "Pinned"}, token=A, expect=[200, 204])
    secs2 = as_list(req("GET", f"/api/workspaces/{W}/sections", token=A), "sections")
    mine2 = next((s for s in secs2 if s.get("id") == Sid), None)
    if not mine2 or mine2.get("name") != "Pinned":
        fail(f"rename/remove not reflected: {mine2}")
    ok("channel removed + section renamed")
    # Privacy: bob sees none of alice's sections and cannot mutate them.
    bsecs = as_list(req("GET", f"/api/workspaces/{W}/sections", token=B), "sections")
    if any(s.get("id") == Sid for s in bsecs):
        fail(f"section leaked across users: {bsecs}")
    req("PATCH", f"/api/sections/{Sid}", {"name": "hijack"}, token=B, expect=[403, 404])
    ok("sections are private per-user (bob can't see or rename alice's)")
    req("DELETE", f"/api/sections/{Sid}", token=A, expect=[200, 204])
    secs3 = as_list(req("GET", f"/api/workspaces/{W}/sections", token=A), "sections")
    if any(s.get("id") == Sid for s in secs3):
        fail("section not deleted")
    ok("section deleted (items cascade)")

    # ---------------- Saved searches ----------------
    say("saved searches: save, list, run (returns matching messages), delete; owner-scoped")
    token_word = f"zorptoken{ts}"
    rs = req("POST", "/api/rooms", {"kind": "group", "name": f"search-room-{ts}", "workspace_id": W}, token=A)["id"]
    # Post a message containing the unique token via the slash-command send path.
    req("POST", f"/api/rooms/{rs}/command", {"text": f"/me mentions {token_word} in passing"}, token=A, expect=200)
    time.sleep(0.3)
    saved = req("POST", f"/api/workspaces/{W}/saved-searches",
                {"name": "token hunt", "query": token_word}, token=A)
    SSid = saved.get("id") or saved.get("saved_search", {}).get("id")
    if not SSid:
        fail(f"no saved-search id: {saved}")
    lst = as_list(req("GET", f"/api/workspaces/{W}/saved-searches", token=A), "saved_searches", "searches")
    if not any(s.get("id") == SSid for s in lst):
        fail(f"saved search not listed: {lst}")
    ok(f"saved search created + listed ({len(lst)})")
    run = req("POST", f"/api/saved-searches/{SSid}/run", token=A)
    hits = as_list(run, "results", "messages", "hits")
    blob = json.dumps(run)
    if token_word not in blob:
        fail(f"run did not surface the token message: {blob[:300]}")
    ok(f"saved search run returned the matching message ({len(hits)} hit(s))")
    req("DELETE", f"/api/saved-searches/{SSid}", token=B, expect=[403, 404])
    req("DELETE", f"/api/saved-searches/{SSid}", token=A, expect=[200, 204])
    ok("owner-scoped delete (stranger blocked; owner deletes)")

    # ---------------- Message-anchored reminders ----------------
    say("message reminders: 'remind me about this message' schedules a durable reminder")
    rr = req("POST", "/api/rooms", {"kind": "group", "name": f"msgrem-{ts}"}, token=A)["id"]
    posted = req("POST", f"/api/rooms/{rr}/command", {"text": "/me discuss the Q3 budget"}, token=A)
    Mid = posted.get("id")
    if not Mid:
        fail(f"no message id: {posted}")
    res = req("POST", f"/api/messages/{Mid}/remind", {"in": "1h"}, token=A)
    if not res.get("scheduled") or not res.get("reminder_id"):
        fail(f"message reminder not scheduled: {res}")
    ok(f"reminder scheduled (deliver_at={res.get('deliver_at')})")
    pend = req("GET", f"/api/messages/{Mid}/reminders", token=A)
    plist = pend if isinstance(pend, list) else as_list(pend, "scheduled", "reminders")
    if not plist:
        fail(f"reminder not pending: {pend}")
    ok(f"reminder is pending in the scheduler ({len(plist)})")
    req("POST", f"/api/messages/{Mid}/remind", {"in": "whenever"}, token=A, expect=400)
    req("POST", f"/api/messages/{Mid}/remind", {"in": "1h"}, token=B, expect=403)
    ok("unparseable time rejected (400); non-member forbidden (403)")

    print("\n\033[1;32m✅ Wave-9 smoke PASSED (channel sections, saved searches, message reminders)\033[0m")


if __name__ == "__main__":
    main()
