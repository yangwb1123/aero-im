#!/usr/bin/env python3
"""Wave-6 smoke: polls, stream chat moderation, stream VOD. (Unfurl is covered by
unit tests; its live path needs an external URL, so it's not smoked here.)"""
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
    data = json.dumps(body).encode() if body is not None else None
    if body is not None: headers["content-type"] = "application/json"
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect: fail(f"{method} {path}: want {expect} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect: fail(f"{method} {path}: want {expect} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


async def expect_ws_chat_forbidden(token, stream_id):
    async with websockets.connect(f"{WS_HOST}/ws?token={token}") as ws:
        welcome = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
        if welcome.get("type") != "welcome":
            fail(f"unexpected WS welcome: {welcome}")
        await ws.send(json.dumps({
            "type": "stream_chat",
            "stream_id": stream_id,
            "body": "banned websocket post",
        }))
        for _ in range(6):
            frame = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if frame.get("type") == "error":
                if "banned from this stream" not in frame.get("msg", ""):
                    fail(f"WS chat rejected for the wrong reason: {frame}")
                return
        fail("banned viewer's WS chat did not return an error")


def main():
    ts = int(time.time())
    a = req("POST", "/api/auth/register", {"email": f"a_w6+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceW6"})
    b = req("POST", "/api/auth/register", {"email": f"b_w6+{ts}@aero.dev", "password": "password_1234", "display_name": "BobW6"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"w6-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Bpid}, token=A)
    ok(f"setup: alice/bob + room {Rid[:8]}")

    # ---------------- Polls ----------------
    say("polls: create, both vote, tally reflects votes, close")
    poll = req("POST", f"/api/rooms/{Rid}/polls",
               {"question": "Lunch?", "options": ["Pizza", "Sushi", "Tacos"], "multi": False}, token=A)
    Pid = poll["id"]
    req("POST", f"/api/polls/{Pid}/vote", {"option_idx": 1}, token=A)   # Alice → Sushi
    req("POST", f"/api/polls/{Pid}/vote", {"option_idx": 1}, token=B)   # Bob → Sushi
    tally = req("GET", f"/api/polls/{Pid}", token=A)
    counts = tally.get("counts") or tally.get("tally") or tally.get("poll", {}).get("counts")
    # tolerate response shape: find the per-option counts
    if counts is None:
        # try common shapes
        counts = tally.get("votes") or tally.get("results")
    if not counts: fail(f"no tally in response: {tally}")
    if counts[1] != 2: fail(f"expected 2 votes on option 1, got {counts}")
    ok(f"poll tally correct: {counts}")
    say("polls: single-choice revote replaces; close stops voting")
    req("POST", f"/api/polls/{Pid}/vote", {"option_idx": 0}, token=A)   # Alice changes → Pizza
    tally2 = req("GET", f"/api/polls/{Pid}", token=A)
    c2 = tally2.get("counts") or tally2.get("votes") or tally2.get("results")
    if c2[0] != 1 or c2[1] != 1: fail(f"revote didn't replace: {c2}")
    ok(f"revote replaced prior choice: {c2}")
    req("POST", f"/api/polls/{Pid}/close", token=A, expect=200)
    req("POST", f"/api/polls/{Pid}/vote", {"option_idx": 2}, token=B, expect=409)  # closed → conflict
    ok("closed poll rejects further votes")

    # ---------------- Stream chat moderation ----------------
    say("stream moderation: owner bans a viewer; their chat is rejected")
    stream = req("POST", "/api/streams", {"title": f"w6-stream-{ts}", "room_id": Rid, "protocol": "rtmp"}, token=A)
    Sid = stream["id"]
    # Bob can chat before the ban.
    req("POST", f"/api/streams/{Sid}/chat", {"body": "hi from bob"}, token=B, expect=200)
    ok("bob can chat pre-ban")
    req("POST", f"/api/streams/{Sid}/ban", {"participant_id": Bpid, "reason": "spam"}, token=A, expect=200)
    req("POST", f"/api/streams/{Sid}/chat", {"body": "spam spam"}, token=B, expect=403)
    asyncio.run(expect_ws_chat_forbidden(B, Sid))
    ok("banned viewer's REST + WS chat rejected")
    bans = req("GET", f"/api/streams/{Sid}/bans", token=A)
    if not any((x.get("participant_id") == Bpid) for x in (bans if isinstance(bans, list) else bans.get("bans", []))):
        fail(f"ban not listed: {bans}")
    req("POST", f"/api/streams/{Sid}/unban", {"participant_id": Bpid}, token=A, expect=200)
    req("POST", f"/api/streams/{Sid}/chat", {"body": "back again"}, token=B, expect=200)
    ok("unban restores chat")
    say("stream moderation: a non-owner cannot ban")
    req("POST", f"/api/streams/{Sid}/ban", {"participant_id": Apid}, token=B, expect=403)
    ok("non-owner ban forbidden")

    # ---------------- Stream VOD ----------------
    say("VOD: recording lifecycle is wired; finalize honors the no-real-media seam")
    # The recording flag toggles (lifecycle).
    req("POST", f"/api/streams/{Sid}/record", {"on": True}, token=A, expect=200)
    req("POST", f"/api/streams/{Sid}/end", token=A)
    # A VOD needs the stream's HLS playlist, written by the real ingest pipeline
    # (absent in-sandbox). So the explicit finalize correctly reports no media with
    # a 400 — proving the route is wired + the seam is honored. (VOD-record
    # create/list with a real hls_path is verified by the storage db_tests.)
    req("POST", f"/api/streams/{Sid}/vod", token=A, expect=400)
    vods = req("GET", f"/api/streams/{Sid}/vods", token=A)
    vlist = vods if isinstance(vods, list) else vods.get("vods", [])
    ok(f"VOD routes wired; finalize honors no-media seam ({len(vlist)} VOD(s) without a real push)")

    print("\n\033[1;32m✅ Wave-6 smoke PASSED (polls vote/tally/close, stream ban/unban, VOD lifecycle)\033[0m")


if __name__ == "__main__":
    main()
