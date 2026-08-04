#!/usr/bin/env python3
"""P3 live-caption smoke: verifies the server-side caption relay.

The browser side uses the Web Speech API; this exercises the part the server
owns — a `call_caption` client frame is relayed to the room as a
`call`/`op:"caption"` server frame (with the optional AI translation on final
lines when a target language is set and ANTHROPIC_API_KEY is configured).

Two WS clients join a room and establish a call; client A emits a caption;
client B must receive it.

Requires the stack up (Postgres/Redis/NATS) and the server on $AERO_HOST.
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


def say(msg): print(f"\033[1;36m▶ {msg}\033[0m")
def ok(msg): print(f"  \033[1;32m✓ {msg}\033[0m")
def fail(msg): print(f"  \033[1;31m✗ {msg}\033[0m"); sys.exit(1)


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


async def main():
    ts = int(time.time())
    say("health")
    req("GET", "/health")
    ok("up")

    say("register A (speaker) + B (listener)")
    a = req("POST", "/api/auth/register", {"email": f"capA+{ts}@aero.dev", "password": "password_1234", "display_name": "CapA"})
    b = req("POST", "/api/auth/register", {"email": f"capB+{ts}@aero.dev", "password": "password_1234", "display_name": "CapB"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"cap-{ts}"}, token=A)["id"]
    req("POST", f"/api/rooms/{room}/members", {"participant_id": Bpid}, token=A)
    ok(f"room={room[:8]}")

    say("B joins; A emits a final caption; B must receive it")
    async with websockets.connect(f"{WS_HOST}/ws?token={B}") as wb:
        assert json.loads(await wb.recv())["type"] == "welcome"
        await wb.send(json.dumps({"type": "join_room", "room_id": room}))
        await wb.recv()  # presence
        async with websockets.connect(f"{WS_HOST}/ws?token={A}") as wa:
            assert json.loads(await wa.recv())["type"] == "welcome"
            await wa.send(json.dumps({"type": "join_room", "room_id": room}))
            await wa.recv()
            sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n"
            await wa.send(json.dumps({
                "type": "call_invite",
                "room_id": room,
                "kind": "audio",
                "sdp": sdp,
            }))
            invite = None
            for _ in range(8):
                frame = json.loads(await asyncio.wait_for(wb.recv(), timeout=3))
                event = frame.get("event", {})
                if frame.get("type") == "call" and event.get("op") == "invite":
                    invite = event
                    break
            if not invite or not invite.get("call_id"):
                fail(f"B did not receive the call invite: {invite}")
            call_id = invite["call_id"]
            await wb.send(json.dumps({
                "type": "call_answer",
                "call_id": call_id,
                "room_id": room,
                "to": Apid,
                "sdp": sdp,
            }))
            await asyncio.sleep(0.2)
            await wa.send(json.dumps({
                "type": "call_caption",
                "call_id": call_id,
                "room_id": room, "text": "你好世界", "lang": "zh-CN", "is_final": True,
            }))
            got = None
            for _ in range(8):
                f = json.loads(await asyncio.wait_for(wb.recv(), timeout=3))
                if f.get("type") == "call" and f.get("event", {}).get("op") == "caption":
                    got = f["event"]
                    break
            if not got:
                fail("B did not receive the caption")
            if got.get("text") != "你好世界":
                fail(f"caption text mismatch: {got}")
            if got.get("from") != Apid:
                fail(f"caption from mismatch: {got}")
            if got.get("is_final") is not True:
                fail(f"caption is_final mismatch: {got}")
            ok(f"caption relayed B←A: text={got['text']!r} final={got['is_final']}")
            await wa.send(json.dumps({
                "type": "call_end",
                "call_id": call_id,
                "room_id": room,
            }))

    print()
    print("\033[1;32m✓ captions (P3) smoke passed\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
