#!/usr/bin/env python3
"""Wave-1 collaboration smoke: threads, @mentions → notifications + unread, pins.

Exercises the new REST + WS surface end-to-end against a running server:
  - Alice mentions Bob and replies to Bob's message  → Bob gets notifications
  - per-room unread + mention counts                  → /api/unread
  - mark notifications read                           → unread drops
  - thread fetch                                      → /api/messages/:id/thread
  - pin / list / unpin                                → /api/rooms/:id/pins
  - realtime: Bob's WS receives `notify`; room receives `pin`
"""
from __future__ import annotations

import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None):
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
            if resp.status == 204 or not buf: return None
            return json.loads(buf)
    except urllib.error.HTTPError as e:
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


async def ws_send(token, room_id, blocks, reply_to=None):
    """Open WS, send one message, return (message, frames_seen)."""
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room_id}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({
            "type": "send_message", "room_id": room_id, "blocks": blocks, "reply_to": reply_to
        }))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message":
                return f["message"]
    fail("no message echo via WS")


async def main():
    ts = int(time.time())
    say("register Alice + Bob")
    a = req("POST", "/api/auth/register", {"email": f"a_col+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceCol"})
    b = req("POST", "/api/auth/register", {"email": f"b_col+{ts}@aero.dev", "password": "password_1234", "display_name": "BobCol"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    ok(f"alice={Apid[:8]} bob={Bpid[:8]}")

    say("alice creates room + invites bob")
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"col-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={Rid[:8]}")

    # Bob opens a WS and listens for a `notify` frame in the background.
    notify_box = {}
    async def bob_listen():
        url = f"{WS_HOST}/ws?token={B}"
        async with websockets.connect(url) as ws:
            assert json.loads(await ws.recv())["type"] == "welcome"
            try:
                while True:
                    f = json.loads(await asyncio.wait_for(ws.recv(), timeout=8))
                    if f.get("type") == "notify":
                        notify_box["frame"] = f
                        return
            except asyncio.TimeoutError:
                return
    bob_task = asyncio.create_task(bob_listen())
    await asyncio.sleep(0.5)  # let Bob's socket register

    say("bob posts a message (so alice can reply to it)")
    bob_msg = await ws_send(B, Rid, [{"type": "text", "content": "bob says hi"}])
    Bob_Mid = bob_msg["id"]
    ok(f"bob msg={Bob_Mid[:8]}")

    say("alice @-mentions bob AND replies to bob's message")
    alice_msg = await ws_send(
        A, Rid,
        [{"type": "text", "content": "hey "}, {"type": "mention", "participant": Bpid}],
        reply_to=Bob_Mid,
    )
    Alice_Mid = alice_msg["id"]
    ok(f"alice msg={Alice_Mid[:8]} (mention+reply)")

    say("bob's WS should receive a realtime `notify`")
    await asyncio.wait_for(bob_task, timeout=10)
    nf = notify_box.get("frame")
    if not nf: fail("bob did not receive a notify frame")
    if nf.get("mentioned") != Bpid: fail(f"notify.mentioned != bob: {nf}")
    if nf.get("notify_kind") != "mention": fail(f"expected mention kind, got {nf}")
    ok(f"notify received: kind={nf['notify_kind']} by={nf['by'][:8]}")

    say("bob's notification inbox + unread count")
    await asyncio.sleep(0.3)
    inbox = req("GET", "/api/notifications", token=B)
    if not isinstance(inbox, list) or len(inbox) < 1: fail(f"empty inbox: {inbox}")
    kinds = {n["kind"] for n in inbox}
    # Alice's message both mentions and replies to Bob → mention wins (one row).
    if "mention" not in kinds: fail(f"no mention in inbox: {kinds}")
    cnt = req("GET", "/api/notifications/count", token=B)
    if cnt["unread"] < 1: fail(f"unread count 0: {cnt}")
    ok(f"inbox={len(inbox)} kinds={kinds} unread={cnt['unread']}")

    say("per-room unread (messages + mentions)")
    unread = req("GET", "/api/unread", token=B)
    mine = [u for u in unread if u["room_id"] == Rid]
    if not mine: fail(f"room not in unread: {unread}")
    u = mine[0]
    if u["mentions"] < 1: fail(f"mentions==0: {u}")
    if u["unread"] < 1: fail(f"unread==0: {u}")
    ok(f"room unread: msgs={u['unread']} mentions={u['mentions']}")

    say("bob marks notifications read → unread drops to 0")
    req("POST", "/api/notifications/read", {"all": True}, token=B)
    cnt2 = req("GET", "/api/notifications/count", token=B)
    if cnt2["unread"] != 0: fail(f"unread not cleared: {cnt2}")
    ok("inbox cleared")

    say("thread fetch: alice's reply is in bob's message thread")
    th = req("GET", f"/api/messages/{Bob_Mid}/thread", token=A)
    if th["summary"]["reply_count"] < 1: fail(f"thread empty: {th['summary']}")
    rids = {r["id"] for r in th["replies"]}
    if Alice_Mid not in rids: fail(f"reply not in thread: {rids}")
    ok(f"thread: {th['summary']['reply_count']} replies, repliers={len(th['summary']['repliers'])}")

    say("pin alice's message; verify list; realtime pin frame to room")
    pin_box = {}
    async def alice_listen_pin():
        url = f"{WS_HOST}/ws?token={A}"
        async with websockets.connect(url) as ws:
            assert json.loads(await ws.recv())["type"] == "welcome"
            await ws.send(json.dumps({"type": "join_room", "room_id": Rid}))
            try:
                while True:
                    f = json.loads(await asyncio.wait_for(ws.recv(), timeout=8))
                    if f.get("type") == "pin":
                        pin_box["frame"] = f
                        return
            except asyncio.TimeoutError:
                return
    pin_task = asyncio.create_task(alice_listen_pin())
    await asyncio.sleep(0.5)

    pinned = req("POST", f"/api/rooms/{Rid}/pins", {"message_id": Alice_Mid}, token=B)
    if not pinned.get("pinned"): fail(f"pin failed: {pinned}")
    ok(f"pinned (created={pinned['created']})")

    await asyncio.wait_for(pin_task, timeout=10)
    pf = pin_box.get("frame")
    if not pf or pf.get("op") != "pin" or pf.get("message_id") != Alice_Mid:
        fail(f"bad/missing pin frame: {pf}")
    ok(f"pin frame received: op={pf['op']}")

    pins = req("GET", f"/api/rooms/{Rid}/pins", token=A)
    if not any(p["message"]["id"] == Alice_Mid for p in pins): fail(f"pin not listed: {pins}")
    ok(f"{len(pins)} pin(s) listed with joined message content")

    say("unpin → list empty")
    req("DELETE", f"/api/rooms/{Rid}/pins/{Alice_Mid}", token=A)
    pins2 = req("GET", f"/api/rooms/{Rid}/pins", token=A)
    if any(p["message"]["id"] == Alice_Mid for p in pins2): fail("still pinned after unpin")
    ok("unpinned")

    print("\n\033[1;32m✅ Wave-1 collab smoke PASSED\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
