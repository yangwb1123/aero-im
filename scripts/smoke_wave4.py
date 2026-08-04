#!/usr/bin/env python3
"""Wave-4 smoke: scheduled messages, invitations, cross-room search, mute/DND.

Run against a live server. The mute/DND suppression seam is verified end-to-end
(a muted/DND recipient receives NO notification; lifting it restores delivery).
"""
from __future__ import annotations
import asyncio, json, os, sys, time, datetime, urllib.error, urllib.parse, urllib.request
import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")


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
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect:
                fail(f"{method} {path}: expected {expect} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect: fail(f"{method} {path}: expected {expect} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


async def ws_send(token, room_id, blocks, reply_to=None):
    async with websockets.connect(f"{WS_HOST}/ws?token={token}") as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room_id}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({"type": "send_message", "room_id": room_id, "blocks": blocks, "reply_to": reply_to}))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message": return f["message"]
    fail("no message echo")


def main():
    ts = int(time.time())
    a = req("POST", "/api/auth/register", {"email": f"a_w4+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceW4"})
    b = req("POST", "/api/auth/register", {"email": f"b_w4+{ts}@aero.dev", "password": "password_1234", "display_name": "BobW4"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"w4-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Bpid}, token=A)
    ok(f"setup: alice/bob + room {Rid[:8]}")

    # ---------------- Scheduled messages ----------------
    say("scheduled: schedule a message ~2s out; dispatcher delivers it")
    when = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=2)).isoformat().replace("+00:00", "Z")
    sched = req("POST", f"/api/rooms/{Rid}/scheduled",
                {"blocks": [{"type": "text", "content": f"scheduled hello {ts}"}], "scheduled_at": when}, token=A)
    pend = req("GET", f"/api/rooms/{Rid}/scheduled", token=A)
    if not any(s["id"] == sched["id"] for s in pend): fail(f"scheduled not pending: {pend}")
    ok(f"scheduled id={sched['id'][:8]}, pending list OK")
    say("scheduled: past scheduled_at is rejected")
    past = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=1)).isoformat().replace("+00:00", "Z")
    req("POST", f"/api/rooms/{Rid}/scheduled", {"blocks": [{"type": "text", "content": "x"}], "scheduled_at": past}, token=A, expect=400)
    ok("past schedule rejected (400)")
    say("scheduled: waiting for delivery (dispatcher polls ~10s)...")
    delivered = False
    for _ in range(16):
        time.sleep(1)
        hist = req("GET", f"/api/rooms/{Rid}/messages?limit=30", token=A)
        if any(f"scheduled hello {ts}" in blk.get("content", "") for m in hist for blk in m.get("blocks", [])):
            delivered = True; break
    if not delivered: fail("scheduled message was not delivered within 16s")
    ok("scheduled message delivered to the room")

    # ---------------- Invitations ----------------
    say("invitations: alice creates a workspace + an invite link; bob accepts")
    ws = req("POST", "/api/workspaces", {"name": f"W4 Org {ts}", "slug": f"w4org-{ts}"}, token=A)
    Wsid = ws["id"]
    inv = req("POST", f"/api/workspaces/{Wsid}/invitations", {"role": "member", "max_uses": 5}, token=A)
    tok = inv.get("token")
    if not tok: fail(f"no invite token: {inv}")
    ok(f"invite created; url={inv.get('invite_url','')[:48]}…")
    acc = req("POST", "/api/invitations/accept", {"token": tok}, token=B)
    if acc.get("workspace_id") != Wsid: fail(f"accept did not join ws: {acc}")
    members = req("GET", f"/api/workspaces/{Wsid}/members", token=A)
    if not any(m["participant_id"] == Bpid for m in members): fail("bob not a ws member after accept")
    ok("bob accepted the invite → workspace member")
    say("invitations: a revoked invite cannot be accepted")
    inv2 = req("POST", f"/api/workspaces/{Wsid}/invitations", {"role": "member"}, token=A)
    req("DELETE", f"/api/invitations/{inv2['id']}", token=A)
    code = req("POST", "/api/invitations/accept", {"token": inv2["token"]}, token=B, expect=None) if False else None
    try:
        urllib.request.urlopen(urllib.request.Request(HOST + "/api/invitations/accept", method="POST",
            data=json.dumps({"token": inv2["token"]}).encode(),
            headers={"content-type": "application/json", "authorization": f"Bearer {B}"}))
        c = 200
    except urllib.error.HTTPError as e:
        c = e.code
    if c == 200: fail("revoked invite was accepted")
    ok(f"revoked invite rejected (status {c})")

    # ---------------- Cross-room search ----------------
    say("search: messages across the caller's rooms are found via /api/search")
    asyncio.run(ws_send(A, Rid, [{"type": "text", "content": f"zylophone-{ts} meeting notes"}]))
    time.sleep(0.4)
    res = req("POST", "/api/search", {"query": f"zylophone-{ts}", "limit": 10}, token=A)
    if not res.get("results"): fail(f"search found nothing: {res}")
    if not any(f"zylophone-{ts}" in blk.get("content", "") for r in res["results"] for blk in r["message"].get("blocks", [])):
        fail(f"search hit missing needle: {res}")
    ok(f"cross-room search returned {len(res['results'])} hit(s)")

    # ---------------- Mute / DND (the suppression seam) ----------------
    def bob_unread():
        return req("GET", "/api/notifications/count", token=B)["unread"]

    say("mute: a muted room suppresses bob's mention notification")
    req("POST", "/api/notifications/read", {"all": True}, token=B)  # clear inbox
    base = bob_unread()
    req("POST", f"/api/rooms/{Rid}/mute", token=B, expect=200)
    asyncio.run(
        ws_send(
            A,
            Rid,
            [
                {"type": "text", "content": "muted "},
                {"type": "mention", "participant": Bpid},
            ],
        )
    )
    time.sleep(0.5)
    if bob_unread() != base: fail(f"muted mention still notified (base={base}, now={bob_unread()})")
    ok("muted: no notification delivered")

    say("unmute: bob is notified again")
    req("DELETE", f"/api/rooms/{Rid}/mute", token=B, expect=200)
    asyncio.run(
        ws_send(
            A,
            Rid,
            [
                {"type": "text", "content": "unmuted "},
                {"type": "mention", "participant": Bpid},
            ],
        )
    )
    time.sleep(0.5)
    if bob_unread() <= base: fail(f"unmuted mention not notified (base={base}, now={bob_unread()})")
    ok(f"unmuted: notification delivered (unread {base}→{bob_unread()})")

    say("DND: a window covering now suppresses; clearing restores")
    req("POST", "/api/notifications/read", {"all": True}, token=B)
    base2 = bob_unread()
    now_min = datetime.datetime.now(datetime.timezone.utc).hour * 60 + datetime.datetime.now(datetime.timezone.utc).minute
    req("PUT", "/api/notifications/prefs/dnd", {"start_minute": now_min, "end_minute": (now_min + 1) % 1440}, token=B, expect=200)
    prefs = req("GET", "/api/notifications/prefs", token=B)
    if not prefs.get("dnd"): fail(f"DND not set: {prefs}")
    asyncio.run(
        ws_send(
            A,
            Rid,
            [
                {"type": "text", "content": "dnd "},
                {"type": "mention", "participant": Bpid},
            ],
        )
    )
    time.sleep(0.5)
    if bob_unread() != base2: fail(f"DND mention still notified (base={base2}, now={bob_unread()})")
    ok("DND: no notification during window")
    req("PUT", "/api/notifications/prefs/dnd", {}, token=B, expect=200)  # clear
    asyncio.run(
        ws_send(
            A,
            Rid,
            [
                {"type": "text", "content": "post-dnd "},
                {"type": "mention", "participant": Bpid},
            ],
        )
    )
    time.sleep(0.5)
    if bob_unread() <= base2: fail("after clearing DND, mention not notified")
    ok("DND cleared: notification delivered again")

    print("\n\033[1;32m✅ Wave-4 smoke PASSED (scheduled deliver, invitations accept/revoke, search, mute+DND suppression)\033[0m")


if __name__ == "__main__":
    main()
