#!/usr/bin/env python3
"""Wave-22 smoke: missed-call notifications. A 1:1 call that ends without ever
being answered drops a durable "call_missed" entry into each callee's activity
feed (Wave 21); an answered call does not.

Two WS clients (caller + callee). Requires the stack up + server on $AERO_HOST.
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
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
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:200]}")


def missed_for(token, call_id):
    feed = req("GET", "/api/activity", token=token)
    rows = feed if isinstance(feed, list) else feed.get("entries", [])
    return [e for e in rows if e.get("kind") == "call_missed" and e.get("subject_id") == call_id]


async def recv_call(ws, op, timeout=4):
    """Read frames until a call event with the given op arrives; return its event."""
    for _ in range(12):
        f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if f.get("type") == "call" and f.get("event", {}).get("op") == op:
            return f["event"]
    return None


async def main():
    ts = int(time.time())
    say("setup: register alice (caller) + bob (callee); shared room; both join WS")
    a = req("POST", "/api/auth/register", {"email": f"mcA+{ts}@aero.dev", "password": "password_1234", "display_name": "McA"})
    b = req("POST", "/api/auth/register", {"email": f"mcB+{ts}@aero.dev", "password": "password_1234", "display_name": "McB"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"mc-{ts}"}, token=A)["id"]
    req("POST", f"/api/rooms/{room}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={room[:8]} (alice caller, bob callee)")

    async with websockets.connect(f"{WS_HOST}/ws?token={B}") as wb, \
               websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        for w in (wb, wa):
            assert json.loads(await w.recv())["type"] == "welcome"
            await w.send(json.dumps({"type": "join_room", "room_id": room}))
            await w.recv()  # presence

        # ---------------- Missed call (no answer) ----------------
        say("missed call: alice rings, bob never answers, alice ends → bob gets a 'call_missed'")
        await wa.send(json.dumps({"type": "call_invite", "room_id": room, "kind": "audio", "sdp": "v=0 dummy"}))
        inv = await recv_call(wb, "invite")
        if not inv or not inv.get("call_id"):
            fail(f"bob did not receive the call invite: {inv}")
        call1 = inv["call_id"]
        await wa.send(json.dumps({"type": "call_end", "call_id": call1, "room_id": room}))
        await asyncio.sleep(0.4)
        missed = missed_for(B, call1)
        if not missed:
            fail("bob did not get a 'call_missed' activity entry for the unanswered call")
        if missed[0].get("actor_id") not in (Apid, None):
            fail(f"missed-call actor mismatch: {missed[0]}")
        ok("unanswered call → bob's activity feed has a 'call_missed' (actor=alice)")

        # ---------------- Answered call (no missed notice) ----------------
        say("answered call: bob answers this time → NO 'call_missed' is recorded")
        await wa.send(json.dumps({"type": "call_invite", "room_id": room, "kind": "audio", "sdp": "v=0 dummy2"}))
        inv2 = await recv_call(wb, "invite")
        call2 = inv2["call_id"]
        await wb.send(json.dumps({"type": "call_answer", "call_id": call2, "room_id": room, "to": Apid, "sdp": "v=0 ans"}))
        await asyncio.sleep(0.3)
        await wa.send(json.dumps({"type": "call_end", "call_id": call2, "room_id": room}))
        await asyncio.sleep(0.4)
        if missed_for(B, call2):
            fail("an ANSWERED call wrongly produced a 'call_missed' entry")
        ok("answered call → no 'call_missed' entry")

    print("\n\033[1;32m✅ Wave-22 smoke PASSED (missed-call notifications: notify-on-unanswered, none-on-answered)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
