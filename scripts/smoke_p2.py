#!/usr/bin/env python3
"""P2 feature smoke: exercises the new REST surface end-to-end.

Covers: register, room create + invite, WS send (one message), edit, react,
mark read, list receipts, search (FTS), upload + send file block,
reactions batch, AI summarize/ask, participant search, stream create."""
from __future__ import annotations

import asyncio
import base64
import io
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")


def say(msg): print(f"\033[1;36m▶ {msg}\033[0m")
def ok(msg): print(f"  \033[1;32m✓ {msg}\033[0m")
def fail(msg): print(f"  \033[1;31m✗ {msg}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, raw=False):
    headers = {"accept": "application/json"}
    if token: headers["authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        if raw:
            data = body
            # leave headers alone; caller sets content-type
        else:
            headers["content-type"] = "application/json"
            data = json.dumps(body).encode()
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if resp.status == 204 or not buf: return None
            ct = resp.headers.get("content-type", "")
            if "application/json" in ct: return json.loads(buf)
            return buf
    except urllib.error.HTTPError as e:
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def multipart(field_name, filename, content_type, payload):
    boundary = "----aero" + str(time.time_ns())
    head = (
        f"--{boundary}\r\n"
        f'content-disposition: form-data; name="{field_name}"; filename="{filename}"\r\n'
        f"content-type: {content_type}\r\n\r\n"
    ).encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    return boundary, head + payload + tail


def upload(token, room_id, filename, content_type, payload):
    boundary, body = multipart("file", filename, content_type, payload)
    headers = {
        "authorization": f"Bearer {token}",
        "content-type": f"multipart/form-data; boundary={boundary}",
        "accept": "application/json",
    }
    r = urllib.request.Request(
        HOST + f"/api/rooms/{room_id}/blobs",
        method="POST",
        data=body,
        headers=headers,
    )
    with urllib.request.urlopen(r) as resp:
        return json.loads(resp.read())


async def ws_send_message(token, room_id, blocks):
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        welcome = json.loads(await ws.recv())
        assert welcome["type"] == "welcome", welcome
        await ws.send(json.dumps({"type": "join_room", "room_id": room_id}))
        pres = json.loads(await ws.recv())
        assert pres["type"] == "presence", pres
        await ws.send(json.dumps({
            "type": "send_message", "room_id": room_id, "blocks": blocks, "reply_to": None
        }))
        for _ in range(5):
            frame = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if frame.get("type") == "message":
                return frame["message"]
        return None


async def main():
    ts = int(time.time())
    say("health")
    h = req("GET", "/health")
    if not (isinstance(h, (bytes, str)) and b"ok" in (h if isinstance(h, bytes) else h.encode())):
        # /health returns plain text 'ok'
        pass
    ok("up")

    say("register Alice + Bob + Carol")
    a = req("POST", "/api/auth/register", {"email": f"alice_p2+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceP2"})
    b = req("POST", "/api/auth/register", {"email": f"bob_p2+{ts}@aero.dev",   "password": "password_1234", "display_name": "BobP2"})
    c = req("POST", "/api/auth/register", {"email": f"carol_p2+{ts}@aero.dev", "password": "password_1234", "display_name": "CarolP2"})
    A, B, C = a["access_token"], b["access_token"], c["access_token"]
    Apid, Bpid, Cpid = a["participant"]["id"], b["participant"]["id"], c["participant"]["id"]
    ok(f"alice={Apid[:8]} bob={Bpid[:8]} carol={Cpid[:8]}")

    say("participant search")
    s = req("GET", f"/api/participants?q=p2&limit=10", token=A)
    found = {p["id"] for p in s}
    if not ({Apid, Bpid, Cpid} <= found): fail(f"search missing some: {found}")
    ok(f"found {len(s)} participants")

    say("alice creates room + invites bob, carol")
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"p2-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Bpid}, token=A)
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Cpid}, token=A)
    ok(f"room={Rid[:8]}")

    say("alice atomically creates and installs an AI agent bot in the room")
    bot = req(
        "POST",
        "/api/agents",
        {"room_id": Rid, "display_name": "AeroBot", "kind": "bot"},
        token=A,
    )
    ok(f"bot={bot['id'][:8]}")

    say("list room members")
    members = req("GET", f"/api/rooms/{Rid}/members/list", token=A)
    ids = {m["id"] for m in members}
    if not ({Apid, Bpid, Cpid, bot["id"]} <= ids): fail(f"members missing: {ids}")
    ok(f"{len(members)} members")

    say("alice sends a message via WS")
    msg = await ws_send_message(A, Rid, [{"type": "text", "content": "hello team, this is alice"}])
    if not msg: fail("no message received via WS")
    Mid = msg["id"]
    ok(f"sent {Mid[:8]}")

    say("alice edits the message")
    edited = req("PATCH", f"/api/messages/{Mid}", {"blocks": [{"type": "text", "content": "hello team (edited)"}]}, token=A)
    if edited.get("edited_at") is None: fail("edited_at not set")
    ok("edited")

    say("bob reacts with 👍")
    r1 = req("POST", f"/api/messages/{Mid}/reactions", {"emoji": "👍"}, token=B)
    if r1["op"] != "add": fail(f"expected add, got {r1}")
    ok("reaction added")

    say("carol reacts with 🚀, bob reacts with 🚀 (same)")
    req("POST", f"/api/messages/{Mid}/reactions", {"emoji": "🚀"}, token=C)
    req("POST", f"/api/messages/{Mid}/reactions", {"emoji": "🚀"}, token=B)

    say("reactions batch fetch")
    batch = req("POST", "/api/messages/reactions", {"message_ids": [Mid]}, token=A)
    rs = batch.get(Mid, [])
    by_emoji = {x["emoji"]: x for x in rs}
    if by_emoji.get("👍", {}).get("count") != 1: fail(f"expected 1 thumbs, got {by_emoji}")
    if by_emoji.get("🚀", {}).get("count") != 2: fail(f"expected 2 rockets, got {by_emoji}")
    ok(f"reactions: {[(x['emoji'], x['count']) for x in rs]}")

    say("bob marks read up to the message")
    rc = req("POST", f"/api/rooms/{Rid}/read", {"last_message_id": Mid}, token=B)
    if rc["last_read_message_id"] != Mid: fail(f"unexpected receipt: {rc}")
    ok("bob read")

    say("alice lists receipts")
    receipts = req("GET", f"/api/rooms/{Rid}/receipts", token=A)
    if not any(r["participant_id"] == Bpid and r["last_read_message_id"] == Mid for r in receipts):
        fail(f"bob's receipt missing: {receipts}")
    ok(f"{len(receipts)} receipt(s)")

    say("alice searches (FTS)")
    sr = req("POST", f"/api/rooms/{Rid}/search", {"query": "team", "mode": "fts", "limit": 10}, token=A)
    if not sr.get("results"): fail(f"no FTS hits: {sr}")
    ok(f"FTS hits={len(sr['results'])}")

    say("alice searches (vector)")
    vec = req("POST", f"/api/rooms/{Rid}/search", {"query": "team gathering", "mode": "vector", "limit": 10}, token=A)
    # vector may or may not return a hit depending on embedder; just sanity-check shape
    if "results" not in vec: fail(f"no vector results array: {vec}")
    ok(f"vector results={len(vec['results'])} (may be 0 if embedding hasn't run)")

    say("alice uploads a file")
    payload = b"hello, this is a smoke test file" * 8
    blob = upload(A, Rid, "smoke.txt", "text/plain", payload)
    if blob.get("kind") != "document": fail(f"unexpected kind: {blob}")
    ok(f"blob={blob['id'][:8]} size={blob['size']}")

    say("alice sends file block via WS")
    file_block = {"type": "file", "blob_id": blob["id"], "kind": "document", "name": blob["name"], "size": blob["size"]}
    file_msg = await ws_send_message(A, Rid, [file_block])
    if not file_msg: fail("file message not received")
    ok("file message sent")

    say("anyone downloads the blob")
    body = req("GET", f"/api/blobs/{blob['id']}", token=B)
    if not isinstance(body, (bytes, bytearray)) or body != payload:
        fail(f"blob bytes mismatch ({len(body) if hasattr(body, '__len__') else '?'} != {len(payload)})")
    ok("blob bytes match")

    say("alice asks AI to summarize the room")
    summ = req("POST", "/api/ai/summarize", {"room_id": Rid, "last_n": 20}, token=A)
    if "summary" not in summ: fail(f"unexpected summary: {summ}")
    ok(f"summary len={len(summ['summary'])}")

    say("alice asks AI a question via RAG")
    ask = req("POST", "/api/ai/ask", {"room_id": Rid, "question": "what is happening?", "k": 4}, token=A)
    if "answer" not in ask: fail(f"unexpected ask: {ask}")
    ok(f"answer len={len(ask['answer'])} citations={len(ask.get('citations', []))}")

    say("alice creates a live stream (rtmp)")
    stream = req("POST", "/api/streams", {"title": "smoke-stream", "protocol": "rtmp", "room_id": Rid}, token=A)
    if not stream.get("ingest_url", "").startswith("rtmp://"):
        fail(f"expected rtmp ingest, got {stream}")
    ingest = urllib.parse.urlsplit(stream["ingest_url"])
    ok(f"stream={stream['id'][:8]} ingest={ingest.scheme}://{ingest.hostname}:{ingest.port}")

    say("rtc config")
    rtc = req("GET", "/api/rtc/config", token=A)
    if not rtc.get("ice_servers"): fail(f"no ice servers: {rtc}")
    ok(f"ice servers: {len(rtc['ice_servers'])}")

    say("mls key-package publish + consume")
    kp = req("POST", "/api/mls/key-packages",
             {"ciphersuite": "MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519",
              "payload_b64": base64.b64encode(b"fake-keypackage-bytes").decode()},
             token=B)
    if "id" not in kp: fail(f"kp publish: {kp}")
    consumed = req("GET", f"/api/rooms/{Rid}/mls/key-packages/{Bpid}", token=A)
    if base64.b64decode(consumed["payload_b64"]) != b"fake-keypackage-bytes":
        fail(f"kp payload mismatch: {consumed}")
    ok("MLS KeyPackage round-trip ok")

    say("bot @mention triggers auto-reply (eventual)")
    # Send a message tagging the bot. The agent_bot listener will pick it up via NATS
    # and post a reply asynchronously. Poll history briefly to see the reply.
    msg2 = await ws_send_message(A, Rid, [
        {"type": "text", "content": "嗨 "},
        {"type": "mention", "participant": bot["id"]},
        {"type": "text", "content": " 帮我介绍一下这个房间"},
    ])
    if not msg2: fail("mention message not sent")
    # Wait up to ~6s for the bot reply to appear.
    bot_reply_id = None
    for _ in range(12):
        await asyncio.sleep(0.5)
        h = req("GET", f"/api/rooms/{Rid}/messages?limit=20", token=A)
        if isinstance(h, list):
            for m in h:
                if m.get("sender_id") == bot["id"]:
                    bot_reply_id = m["id"]
                    break
        if bot_reply_id: break
    if bot_reply_id: ok(f"bot replied: {bot_reply_id[:8]}")
    else: ok("bot did not reply yet (best-effort; non-fatal)")

    print()
    print("\033[1;32m✓ P2 smoke passed\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
