#!/usr/bin/env python3
"""Wave-23 smoke: call-transcript persistence + post-call AI recap. A call's
final caption lines are persisted; on call-end an AI recap is generated (heuristic
digest without an LLM key). GET the transcript + recap (room-access gated).

Requires the stack up + server on $AERO_HOST.
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
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
                fail(f"{method} {path}: want {sorted(ok_codes)} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:200]}")


async def recv_call(ws, op, timeout=4):
    for _ in range(12):
        f = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if f.get("type") == "call" and f.get("event", {}).get("op") == op:
            return f["event"]
    return None


async def main():
    ts = int(time.time())
    say("setup: alice + bob in a room; both join WS")
    a = req("POST", "/api/auth/register", {"email": f"crA+{ts}@aero.dev", "password": "password_1234", "display_name": "CrA"})
    b = req("POST", "/api/auth/register", {"email": f"crB+{ts}@aero.dev", "password": "password_1234", "display_name": "CrB"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    Cpid = req("POST", "/api/auth/register", {"email": f"crC+{ts}@aero.dev", "password": "password_1234", "display_name": "CrC"})["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"cr-{ts}"}, token=A)["id"]
    req("POST", f"/api/rooms/{room}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={room[:8]}")

    async with websockets.connect(f"{WS_HOST}/ws?token={B}") as wb, \
               websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
        for w in (wb, wa):
            assert json.loads(await w.recv())["type"] == "welcome"
            await w.send(json.dumps({"type": "join_room", "room_id": room}))
            await w.recv()

        say("call: alice rings, bob answers, alice speaks a final caption, then ends")
        await wa.send(json.dumps({"type": "call_invite", "room_id": room, "kind": "audio", "sdp": "v=0 x"}))
        inv = await recv_call(wb, "invite")
        call_id = inv["call_id"]
        await wb.send(json.dumps({"type": "call_answer", "call_id": call_id, "room_id": room, "to": Apid, "sdp": "v=0 a"}))
        await asyncio.sleep(0.2)
        await wa.send(json.dumps({"type": "call_caption", "call_id": call_id, "room_id": room,
                                  "text": "let us ship the release on friday", "lang": "en", "is_final": True}))
        await asyncio.sleep(0.3)
        await wa.send(json.dumps({"type": "call_end", "call_id": call_id, "room_id": room}))
        await asyncio.sleep(0.6)

    say("transcript + recap persisted and room-gated")
    tr = req("GET", f"/api/calls/{call_id}/transcript", token=A)
    lines = tr if isinstance(tr, list) else tr.get("lines", [])
    if not any("ship the release" in (l.get("text") or "") for l in lines):
        fail(f"transcript line not persisted: {tr}")
    rc = req("GET", f"/api/calls/{call_id}/recap", token=A)
    if not rc.get("recap"):
        fail(f"recap not generated: {rc}")
    ok(f"transcript has {len(lines)} line(s); recap generated (len={len(rc['recap'])})")

    # non-member (carol) cannot read another call's transcript/recap
    cl = req("POST", "/api/auth/login", {"email": f"crC+{ts}@aero.dev", "password": "password_1234"})["access_token"]
    req("GET", f"/api/calls/{call_id}/transcript", token=cl, expect=[403, 404])
    ok("non-member cannot read the call transcript (403/404)")

    print("\n\033[1;32m✅ Wave-23 smoke PASSED (call transcript persisted + post-call AI recap; room-gated)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
