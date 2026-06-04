#!/usr/bin/env python3
"""Wave-8 smoke: announcement channels (post policy), guest accounts, scheduled
streams, message drafts, and the now-real /remind slash command.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`). Each
section is independent; the whole script exits non-zero on the first mismatch.

Notes on what is exercised vs. seam-honored:
  * Post-policy is asserted through the slash-command REST path
    (`POST /api/rooms/:id/command`), which routes through the SAME
    `ImService::send_message` guard that the WS send path uses — so a 403 there
    proves the enforcement, without needing a WS round-trip.
  * `/remind` schedules a real row via `ScheduledRepo`; delivery happens on the
    background dispatcher's next tick, so we assert the row is staged (visible in
    the room's pending scheduled list) rather than waiting ~10s for delivery.
"""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.request
from datetime import datetime, timedelta, timezone

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    """HTTP helper. `expect` may be an int or an iterable of acceptable codes."""
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
            {"email": f"{tag}_w8+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W8"})
    return r["access_token"], r["participant"]["id"]


def rfc3339(dt):
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def main():
    ts = int(time.time())
    say("setup: register alice/bob/carol; alice creates a workspace she owns")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    C, Cpid = register("carol", ts)
    ws = req("POST", "/api/workspaces", {"name": f"Wave8 {ts}", "slug": f"w8-{ts}"}, token=A)
    W = ws["id"]
    # Bob is a normal member of W (needed so he can be a room member / self-join).
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"},
        token=A, expect=[200, 204])
    ok(f"workspace {W[:8]} owned by alice; bob enrolled as member")

    # ---------------- Announcement channels (post policy) ----------------
    say("post-policy: default 'everyone' lets a member post; 'admins' restricts to admins/creator")
    rann = req("POST", "/api/rooms",
               {"kind": "channel", "name": f"announce-{ts}", "workspace_id": W}, token=A)
    Rann = rann["id"]
    req("POST", f"/api/rooms/{Rann}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    # Default policy: bob can post (via the slash-command send path).
    req("POST", f"/api/rooms/{Rann}/command", {"text": "/me says hi"}, token=B, expect=200)
    ok("default policy: member post allowed")
    # Switch to admins-only.
    req("PUT", f"/api/rooms/{Rann}/post-policy", {"policy": "admins"}, token=A, expect=[200, 204])
    req("POST", f"/api/rooms/{Rann}/command", {"text": "/me tries again"}, token=B, expect=403)
    ok("admins-only: member post rejected (403)")
    req("POST", f"/api/rooms/{Rann}/command", {"text": "/me announces"}, token=A, expect=200)
    ok("admins-only: creator/admin post allowed")
    req("PUT", f"/api/rooms/{Rann}/post-policy", {"policy": "bogus"}, token=A, expect=400)
    ok("invalid policy rejected (400)")
    req("PUT", f"/api/rooms/{Rann}/post-policy", {"policy": "everyone"}, token=A, expect=[200, 204])
    req("POST", f"/api/rooms/{Rann}/command", {"text": "/me back to normal"}, token=B, expect=200)
    ok("reset to everyone: member post allowed again")

    # ---------------- Guest accounts ----------------
    say("guests: admin adds a single-channel guest; guest can't self-join other public channels")
    rpub = req("POST", "/api/rooms",
               {"kind": "channel", "name": f"public-{ts}", "workspace_id": W}, token=A)
    Rpub = rpub["id"]
    req("PATCH", f"/api/rooms/{Rpub}/channel", {"is_private": False}, token=A, expect=[200, 204])
    rguest = req("POST", "/api/rooms",
                 {"kind": "channel", "name": f"guest-room-{ts}", "workspace_id": W}, token=A)
    Rguest = rguest["id"]
    # Non-admin (bob) cannot add a guest.
    req("POST", f"/api/workspaces/{W}/guests", {"participant_id": Cpid, "room_id": Rguest},
        token=B, expect=403)
    ok("non-admin guest-add forbidden (403)")
    # Owner adds carol as a guest scoped to Rguest.
    req("POST", f"/api/workspaces/{W}/guests", {"participant_id": Cpid, "room_id": Rguest},
        token=A, expect=[200, 204, 201])
    members = req("GET", f"/api/rooms/{Rguest}/members/list", token=A)
    if not any(m.get("id") == Cpid for m in (members or [])):
        fail(f"guest not added to target room: {members}")
    glist = req("GET", f"/api/workspaces/{W}/guests", token=A)
    gids = [g.get("participant_id") or g.get("id") for g in
            (glist if isinstance(glist, list) else glist.get("guests", []))]
    if Cpid not in gids:
        fail(f"guest not listed: {glist}")
    ok("owner added guest; guest is in target room + guest list")
    # The guest cannot self-join a public channel; a normal member can.
    req("POST", f"/api/rooms/{Rpub}/join", token=C, expect=403)
    req("POST", f"/api/rooms/{Rpub}/join", token=B, expect=200)
    ok("guest self-join blocked (403); member self-join allowed (200)")
    req("DELETE", f"/api/workspaces/{W}/guests/{Cpid}", token=A, expect=[200, 204])
    ok("guest removed")

    # ---------------- Scheduled streams ----------------
    say("scheduled streams: member announces an upcoming stream; creator cancels; non-member blocked")
    S, _ = register("stranger", ts)  # not a member of W
    future = rfc3339(datetime.now(timezone.utc) + timedelta(hours=2))
    past = rfc3339(datetime.now(timezone.utc) - timedelta(hours=2))
    sch = req("POST", f"/api/workspaces/{W}/scheduled-streams",
              {"title": f"Launch demo {ts}", "description": "Q&A", "scheduled_for": future}, token=A)
    Sid = sch.get("id") or sch.get("scheduled_stream", {}).get("id")
    if not Sid:
        fail(f"no scheduled-stream id: {sch}")
    req("POST", f"/api/workspaces/{W}/scheduled-streams",
        {"title": "too late", "scheduled_for": past}, token=A, expect=400)
    ok("past scheduled_for rejected (400)")
    req("POST", f"/api/workspaces/{W}/scheduled-streams",
        {"title": "nope", "scheduled_for": future}, token=S, expect=403)
    ok("non-member cannot schedule (403)")
    lst = req("GET", f"/api/workspaces/{W}/scheduled-streams", token=A)
    items = lst if isinstance(lst, list) else lst.get("scheduled_streams", lst.get("streams", []))
    if not any((i.get("id") == Sid) for i in items):
        fail(f"scheduled stream not in upcoming list: {lst}")
    ok(f"upcoming list shows it ({len(items)} upcoming)")
    # Non-creator (bob) cannot cancel; creator (alice) can.
    req("DELETE", f"/api/scheduled-streams/{Sid}", token=B, expect=[403, 404])
    req("DELETE", f"/api/scheduled-streams/{Sid}", token=A, expect=[200, 204])
    lst2 = req("GET", f"/api/workspaces/{W}/scheduled-streams", token=A)
    items2 = lst2 if isinstance(lst2, list) else lst2.get("scheduled_streams", lst2.get("streams", []))
    if any((i.get("id") == Sid) for i in items2):
        fail("canceled scheduled stream still upcoming")
    ok("creator canceled; it leaves the upcoming list")

    # ---------------- Message drafts ----------------
    say("drafts: per-room private draft upsert/get/list/delete; another user can't see it")
    rdraft = req("POST", "/api/rooms", {"kind": "group", "name": f"draft-room-{ts}"}, token=A)
    Rd = rdraft["id"]
    req("POST", f"/api/rooms/{Rd}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    req("PUT", f"/api/rooms/{Rd}/draft",
        {"blocks": [{"type": "text", "content": "wip first version"}]}, token=A, expect=[200, 204])
    got = req("GET", f"/api/rooms/{Rd}/draft", token=A)
    draft = got.get("draft") if isinstance(got, dict) else got
    txt = json.dumps(draft)
    if "wip first version" not in txt:
        fail(f"draft not returned: {got}")
    ok("draft saved + fetched")
    # Upsert replaces (still one draft for the room).
    req("PUT", f"/api/rooms/{Rd}/draft",
        {"blocks": [{"type": "text", "content": "wip second version"}]}, token=A, expect=[200, 204])
    got2 = req("GET", f"/api/rooms/{Rd}/draft", token=A)
    if "wip second version" not in json.dumps(got2) or "wip first version" in json.dumps(got2):
        fail(f"upsert did not replace: {got2}")
    ok("upsert replaced prior draft")
    drafts = req("GET", "/api/drafts", token=A)
    dl = drafts if isinstance(drafts, list) else drafts.get("drafts", [])
    if not any((d.get("room_id") == Rd) for d in dl):
        fail(f"draft not in cross-room list: {drafts}")
    ok(f"draft appears in /api/drafts ({len(dl)} draft(s))")
    # Privacy: bob (also a room member) has no draft of his own here.
    bdraft = req("GET", f"/api/rooms/{Rd}/draft", token=B)
    bd = bdraft.get("draft") if isinstance(bdraft, dict) else bdraft
    if bd:
        fail(f"draft leaked across users: {bdraft}")
    ok("drafts are per-user (bob sees none)")
    req("DELETE", f"/api/rooms/{Rd}/draft", token=A, expect=[200, 204])
    after = req("GET", f"/api/rooms/{Rd}/draft", token=A)
    ad = after.get("draft") if isinstance(after, dict) else after
    if ad:
        fail(f"draft not deleted: {after}")
    ok("draft deleted")

    # ---------------- /remind (now real) ----------------
    say("/remind: schedules a durable reminder row (no longer a confirmation-card stub)")
    rrem = req("POST", "/api/rooms", {"kind": "group", "name": f"remind-room-{ts}"}, token=A)
    Rr = rrem["id"]
    res = req("POST", f"/api/rooms/{Rr}/command", {"text": "/remind 30s ship the release"}, token=A)
    if not res.get("scheduled") or not res.get("reminder_id"):
        fail(f"/remind did not schedule: {res}")
    ok(f"reminder scheduled (deliver_at={res.get('deliver_at')})")
    pending = req("GET", f"/api/rooms/{Rr}/scheduled", token=A)
    plist = pending if isinstance(pending, list) else pending.get("scheduled", [])
    if not plist:
        fail(f"reminder not in pending scheduled list: {pending}")
    ok(f"reminder staged in the scheduler ({len(plist)} pending)")
    req("POST", f"/api/rooms/{Rr}/command", {"text": "/remind soon do the thing"}, token=A, expect=400)
    ok("unparseable reminder time rejected (400)")

    print("\n\033[1;32m✅ Wave-8 smoke PASSED "
          "(post-policy, guests, scheduled-streams, drafts, /remind)\033[0m")


if __name__ == "__main__":
    main()
