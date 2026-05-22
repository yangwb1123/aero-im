#!/usr/bin/env python3
"""WebSocket smoke test: register two users, connect both, send + receive a message via NATS fan-out."""
import asyncio
import json
import os
import sys
import time
import urllib.request
import urllib.error

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")

def post(path, body, token=None):
    req = urllib.request.Request(
        HOST + path,
        method="POST",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json"} | ({"authorization": f"Bearer {token}"} if token else {}),
    )
    try:
        with urllib.request.urlopen(req) as r:
            data = r.read()
            return json.loads(data) if data else None
    except urllib.error.HTTPError as e:
        sys.exit(f"HTTP {e.code} {path}: {e.read().decode()}")

async def main():
    ts = int(time.time())
    print("▶ register alice & bob")
    alice = post("/api/auth/register", {"email": f"alice_ws+{ts}@aero.dev", "password": "password_123", "display_name": "AliceWS"})
    bob = post("/api/auth/register", {"email": f"bob_ws+{ts}@aero.dev", "password": "password_123", "display_name": "BobWS"})

    print("▶ alice creates room")
    room = post("/api/rooms", {"kind": "group", "name": f"ws-smoke-{ts}"}, token=alice["access_token"])
    print(f"  room={room['id']}")
    post(f"/api/rooms/{room['id']}/members", {"participant_id": bob["participant"]["id"]}, token=alice["access_token"])

    async def connect(user, label):
        url = f"{WS_HOST}/ws?token={user['access_token']}"
        ws = await websockets.connect(url)
        # Expect welcome
        welcome = json.loads(await ws.recv())
        assert welcome["type"] == "welcome", f"{label} got {welcome}"
        # Join the room
        await ws.send(json.dumps({"type": "join_room", "room_id": room["id"]}))
        # Expect presence
        pres = json.loads(await ws.recv())
        assert pres["type"] == "presence", f"{label} got {pres}"
        print(f"  {label} connected, joined")
        return ws

    print("▶ both connect")
    aw = await connect(alice, "alice")
    bw = await connect(bob, "bob")

    print("▶ alice sends 'hello from alice'")
    await aw.send(json.dumps({
        "type": "send_message",
        "room_id": room["id"],
        "blocks": [{"type": "text", "content": "hello from alice"}],
    }))

    # Both Alice and Bob should receive the message via NATS fan-out.
    async def expect_message(ws, label):
        for _ in range(5):
            frame = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if frame.get("type") == "message":
                blocks = frame["message"]["blocks"]
                assert blocks[0]["content"] == "hello from alice", frame
                print(f"  {label} received: {blocks[0]['content']!r} ✓")
                return
            print(f"  {label} skipped: {frame}")
        raise RuntimeError(f"{label} didn't receive the message")

    await asyncio.gather(expect_message(aw, "alice"), expect_message(bw, "bob"))

    print("▶ bob fetches history (expect 1)")
    req = urllib.request.Request(
        f"{HOST}/api/rooms/{room['id']}/messages",
        headers={"authorization": f"Bearer {bob['access_token']}"},
    )
    with urllib.request.urlopen(req) as r:
        msgs = json.loads(r.read())
    assert len(msgs) == 1, msgs
    print(f"  history len={len(msgs)} ✓")

    await aw.close()
    await bw.close()
    print("\n✓ WS smoke passed — message delivered to both clients via NATS")

asyncio.run(main())
