#!/usr/bin/env python3
"""P11 live-interactivity smoke: danmaku (bullet chat) + virtual gifts + viewers.

Flow: register Host + Fan, create a room + linked rtmp stream, then over a Fan
WebSocket `watch_stream` and assert the server fans out the danmaku / gift /
viewer events that the Host triggers via REST. Finishes with the REST history,
leaderboard, the owner-only end, and the post-end rejection.

Requires the full stack up (Postgres / Redis / NATS) and the server on $AERO_HOST.
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
            if resp.status == 204 or not buf: return None
            ct = resp.headers.get("content-type", "")
            return json.loads(buf) if "application/json" in ct else buf
    except urllib.error.HTTPError as e:
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def status_of(method, path, body=None, token=None):
    """Like req() but returns the HTTP status instead of failing — for negative tests."""
    headers = {"accept": "application/json"}
    if token: headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code


async def watch_and_collect(token, stream_id, trigger):
    """Open a WS, watch the stream, run `trigger` (async) to cause events, and
    collect every `stream_event` frame for a short window."""
    url = f"{WS_HOST}/ws?token={token}"
    events = []
    async with websockets.connect(url) as ws:
        welcome = json.loads(await ws.recv())
        assert welcome["type"] == "welcome", welcome
        await ws.send(json.dumps({"type": "watch_stream", "stream_id": stream_id}))
        await asyncio.sleep(0.5)  # let the watcher register before triggering
        await trigger()
        try:
            while True:
                frame = json.loads(await asyncio.wait_for(ws.recv(), timeout=2.5))
                if frame.get("type") == "stream_event":
                    events.append(frame["event"])
        except asyncio.TimeoutError:
            pass
    return events


async def main():
    ts = int(time.time())

    say("health")
    req("GET", "/health")
    ok("up")

    say("register Host + Fan")
    host = req("POST", "/api/auth/register", {"email": f"host_live+{ts}@aero.dev", "password": "password_1234", "display_name": "HostLive"})
    fan = req("POST", "/api/auth/register", {"email": f"fan_live+{ts}@aero.dev", "password": "password_1234", "display_name": "FanLive"})
    A, F = host["access_token"], fan["access_token"]
    Apid, Fpid = host["participant"]["id"], fan["participant"]["id"]
    ok(f"host={Apid[:8]} fan={Fpid[:8]}")

    say("host creates room + invites fan")
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"live-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Fpid}, token=A)
    ok(f"room={Rid[:8]}")

    say("host goes live (rtmp, linked to room)")
    created = req("POST", "/api/streams", {"title": "live smoke", "protocol": "rtmp", "room_id": Rid}, token=A)
    Sid = created["stream"]["id"]
    ok(f"stream={Sid[:8]}")

    say("gift catalog")
    cat = req("GET", "/api/live/gifts", token=F)
    gifts = {g["id"]: g for g in cat.get("gifts", [])}
    if "rocket" not in gifts: fail(f"catalog missing rocket: {list(gifts)}")
    if gifts["rocket"]["coins"] <= 0: fail("rocket has no price")
    ok(f"{len(gifts)} gifts; rocket={gifts['rocket']['coins']} coins")

    say("fan watches; host fires danmaku + a 2× rocket via REST")

    async def trigger():
        await asyncio.to_thread(req, "POST", f"/api/streams/{Sid}/chat", {"body": "first blood!"}, A)
        await asyncio.to_thread(req, "POST", f"/api/streams/{Sid}/gifts", {"gift_id": "rocket", "qty": 2}, A)

    events = await watch_and_collect(F, Sid, trigger)
    kinds = [e.get("kind") for e in events]
    if "viewers" not in kinds: fail(f"no viewers event delivered: {kinds}")
    chat_ev = next((e for e in events if e.get("kind") == "chat"), None)
    gift_ev = next((e for e in events if e.get("kind") == "gift"), None)
    if not chat_ev or chat_ev.get("body") != "first blood!": fail(f"danmaku not fanned out: {events}")
    if not gift_ev or gift_ev.get("gift_id") != "rocket" or gift_ev.get("qty") != 2:
        fail(f"gift not fanned out: {events}")
    if gift_ev.get("coins") != gifts["rocket"]["coins"] * 2:
        fail(f"gift coins wrong: {gift_ev}")
    ok(f"fan received {len(events)} events: {kinds}")

    say("REST: recent chat contains the danmaku")
    chat = req("GET", f"/api/streams/{Sid}/chat?limit=10", token=F).get("chat", [])
    if not any(c.get("body") == "first blood!" for c in chat): fail(f"chat history missing line: {chat}")
    ok(f"{len(chat)} chat line(s)")

    say("REST: recent gifts + leaderboard")
    glist = req("GET", f"/api/streams/{Sid}/gifts?limit=10", token=F).get("gifts", [])
    if not any(g.get("gift_id") == "rocket" for g in glist): fail(f"gift history missing rocket: {glist}")
    lb = req("GET", f"/api/streams/{Sid}/leaderboard", token=F).get("leaderboard", [])
    top = next((r for r in lb if r["sender_id"] == Apid), None)
    if not top or top["total_coins"] < gifts["rocket"]["coins"] * 2:
        fail(f"leaderboard wrong for host: {lb}")
    ok(f"leaderboard top coins={top['total_coins']}")

    say("negative: fan (non-owner) cannot end the stream")
    code = status_of("POST", f"/api/streams/{Sid}/end", token=F)
    if code != 403: fail(f"expected 403 for non-owner end, got {code}")
    ok("403 as expected")

    say("host ends the stream")
    req("POST", f"/api/streams/{Sid}/end", token=A)
    ok("ended")

    say("negative: chat after end is rejected (409)")
    code = status_of("POST", f"/api/streams/{Sid}/chat", {"body": "too late"}, token=A)
    if code != 409: fail(f"expected 409 posting to ended stream, got {code}")
    ok("409 as expected")

    print()
    print("\033[1;32m✓ live (P11) smoke passed\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
