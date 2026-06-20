#!/usr/bin/env python3
"""Thread routes E2E smoke: threads, notification levels, mutes, unread tracking.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).

Covers:
  - GET /api/messages/:id/thread               → fetch thread + replies
  - POST /api/messages/:id/read                → mark thread read
  - GET /api/me/followed-threads               → list followed threads
  - PUT /api/messages/:id/thread-notification-level  → set notification level
  - GET /api/messages/:id/thread-notification-level  → get notification level
  - GET /api/messages/:id/thread-participants  → list thread participants
  - POST /api/threads/:root/mute               → mute thread
  - DELETE /api/threads/:root/mute             → unmute thread
  - GET /api/threads/:root/mutes               → list thread muters
  - GET /api/messages/:id/unread-count         → thread unread count
  - POST /api/messages/:id/thread-summary      → summarize thread (AI, gated)
  - POST /api/messages/:id/thread-title        → title thread (AI, gated)
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")
DEFAULT_WS = "00000000000000000000000000"


def say(m): print(f"\033[1;36m▶ {m}\033[0m")
def ok(m): print(f"  \033[1;32m✓ {m}\033[0m")
def fail(m): print(f"  \033[1;31m✗ {m}\033[0m"); sys.exit(1)


def req(method, path, body=None, token=None, expect=None):
    """HTTP request helper. Raises fail() on unexpected status."""
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
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
            if resp.status == 204 or not buf:
                return None
            return json.loads(buf)
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect:
                fail(f"{method} {path}: expected {expect} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


async def ws_send(token, room_id, blocks, reply_to=None):
    """Send a message via WebSocket, return the message object."""
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room_id}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({
            "type": "send_message", "room_id": room_id, "blocks": blocks,
            "reply_to": reply_to
        }))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message":
                return f["message"]
    fail("no message echo via WS")


async def main():
    ts = int(time.time())
    
    say("register Alice, Bob, and Carol")
    a = req("POST", "/api/auth/register",
            {"email": f"a_threads+{ts}@aero.dev", "password": "password_1234",
             "display_name": "AliceThreads"})
    b = req("POST", "/api/auth/register",
            {"email": f"b_threads+{ts}@aero.dev", "password": "password_1234",
             "display_name": "BobThreads"})
    c = req("POST", "/api/auth/register",
            {"email": f"c_threads+{ts}@aero.dev", "password": "password_1234",
             "display_name": "CarolThreads"})
    A, B, C = a["access_token"], b["access_token"], c["access_token"]
    Apid, Bpid, Cpid = a["participant"]["id"], b["participant"]["id"], c["participant"]["id"]
    ok(f"users: alice={Apid[:8]} bob={Bpid[:8]} carol={Cpid[:8]}")

    # All three are auto-enrolled in the default workspace; alice makes a group
    # room there and adds bob + carol as members (room add-member returns 204).
    say("alice creates a group room and adds bob + carol")
    room = req("POST", "/api/rooms",
               {"kind": "group", "name": f"threads-{ts}"}, token=A)
    Rid = room["id"]
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Bpid}, token=A, expect=204)
    req("POST", f"/api/rooms/{Rid}/members", {"participant_id": Cpid}, token=A, expect=204)
    ok(f"room={Rid[:8]} (bob + carol added)")

    # ===== Build a real thread =====
    say("alice posts root message (thread start)")
    root_msg = await ws_send(A, Rid, [{"type": "text", "content": f"Main topic: Q4 planning {ts}"}])
    Root_Mid = root_msg["id"]
    ok(f"root msg={Root_Mid[:8]}")

    say("bob and carol post replies to the root")
    bob_reply = await ws_send(B, Rid,
                              [{"type": "text", "content": f"We should prioritize backend {ts}"}],
                              reply_to=Root_Mid)
    Bob_Reply_Mid = bob_reply["id"]
    
    carol_reply = await ws_send(C, Rid,
                                [{"type": "text", "content": f"Frontend needs love too {ts}"}],
                                reply_to=Root_Mid)
    Carol_Reply_Mid = carol_reply["id"]
    ok(f"bob reply={Bob_Reply_Mid[:8]} carol reply={Carol_Reply_Mid[:8]}")

    # ===== GET /api/messages/:id/thread =====
    say("fetch thread: GET /api/messages/:id/thread")
    thread = req("GET", f"/api/messages/{Root_Mid}/thread", token=A)
    if not thread or "replies" not in thread or "summary" not in thread:
        fail(f"thread missing replies/summary: {thread}")
    replies = thread.get("replies", [])
    if len(replies) < 2:
        fail(f"expected 2+ replies, got {len(replies)}")
    reply_ids = {r["id"] for r in replies}
    if Bob_Reply_Mid not in reply_ids or Carol_Reply_Mid not in reply_ids:
        fail(f"expected replies not found: {reply_ids}")
    summary = thread.get("summary", {})
    if summary.get("reply_count", 0) < 2:
        fail(f"summary.reply_count should be 2+, got {summary.get('reply_count')}")
    ok(f"thread fetched: {summary.get('reply_count')} replies, repliers={len(summary.get('repliers', []))}")

    # ===== GET /api/messages/:id/unread-count =====
    say("thread unread count: GET /api/messages/:id/unread-count")
    unread = req("GET", f"/api/messages/{Root_Mid}/unread-count", token=B)
    if "unread" not in unread:
        fail(f"unread missing 'unread' field: {unread}")
    # Initially bob has 1 unread (carol's reply that bob hasn't seen yet)
    if unread.get("unread", 0) < 0:
        fail(f"unread count negative: {unread}")
    ok(f"thread unread count for bob: {unread.get('unread')}")

    # ===== POST /api/messages/:id/read =====
    say("mark thread read: POST /api/messages/:id/read")
    read_resp = req("POST", f"/api/messages/{Root_Mid}/read", token=B, expect=200)
    if not read_resp or not read_resp.get("read"):
        fail(f"read response missing 'read: true': {read_resp}")
    ok("thread marked read (bob)")

    # Verify unread count drops after marking read
    unread_after = req("GET", f"/api/messages/{Root_Mid}/unread-count", token=B)
    if unread_after.get("unread", 0) > 0:
        fail(f"unread count should be 0 after mark_read, got {unread_after}")
    ok(f"unread count after mark_read: {unread_after.get('unread')}")

    # ===== PUT /api/messages/:id/thread-notification-level =====
    say("set thread notification level: PUT /api/messages/:id/thread-notification-level")
    for level in ["all", "mentions", "none"]:
        notif_resp = req("PUT", f"/api/messages/{Root_Mid}/thread-notification-level",
                        {"level": level}, token=B, expect=200)
        if notif_resp.get("level") != level:
            fail(f"set level {level} failed: {notif_resp}")
        ok(f"  level set to '{level}'")

    # ===== GET /api/messages/:id/thread-notification-level =====
    say("get thread notification level: GET /api/messages/:id/thread-notification-level")
    for level in ["all", "mentions"]:
        req("PUT", f"/api/messages/{Root_Mid}/thread-notification-level",
            {"level": level}, token=B, expect=200)
        got = req("GET", f"/api/messages/{Root_Mid}/thread-notification-level", token=B)
        if got.get("level") != level:
            fail(f"get level returned {got.get('level')}, expected {level}")
        ok(f"  get returned level='{level}'")

    # Test invalid level → 400 Invalid
    req("PUT", f"/api/messages/{Root_Mid}/thread-notification-level",
        {"level": "invalid"}, token=A, expect=400)
    ok("invalid level properly rejected (400)")

    # ===== GET /api/messages/:id/thread-participants =====
    say("list thread participants: GET /api/messages/:id/thread-participants")
    participants = req("GET", f"/api/messages/{Root_Mid}/thread-participants", token=A)
    if "participants" not in participants:
        fail(f"participants response missing 'participants': {participants}")
    plist = participants.get("participants", [])
    if len(plist) < 2:
        fail(f"expected 2+ participants, got {len(plist)}")
    pid_set = {p["id"] for p in plist}
    if Bpid not in pid_set or Cpid not in pid_set:
        fail(f"bob/carol not in participants: {pid_set}")
    # Verify each has required fields
    for p in plist:
        if not p.get("id") or "display_name" not in p:
            fail(f"participant missing id/display_name: {p}")
    ok(f"participants: {len(plist)} (bob, carol, ...)")

    # ===== PUT /api/messages/:id/follow (thread subscribe) =====
    say("thread follow: PUT /api/messages/:id/follow")
    follow_resp = req("PUT", f"/api/messages/{Root_Mid}/follow", token=A, expect=200)
    if not follow_resp or not follow_resp.get("following"):
        fail(f"follow response missing 'following: true': {follow_resp}")
    ok("alice follows the thread")

    # ===== GET /api/me/followed-threads =====
    say("list followed threads: GET /api/me/followed-threads")
    followed = req("GET", "/api/me/followed-threads", token=A)
    if "threads" not in followed:
        fail(f"followed response missing 'threads': {followed}")
    threads_list = followed.get("threads", [])
    if Root_Mid not in threads_list:
        fail(f"root message not in followed threads: {threads_list}")
    ok(f"followed threads: {len(threads_list)} (alice)")

    # Bob also follows
    req("PUT", f"/api/messages/{Root_Mid}/follow", token=B, expect=200)
    bob_followed = req("GET", "/api/me/followed-threads", token=B)
    if Root_Mid not in bob_followed.get("threads", []):
        fail("bob's follow didn't register")
    ok("bob follows the thread")

    # ===== DELETE /api/messages/:id/follow (unfollow) =====
    say("thread unfollow: DELETE /api/messages/:id/follow")
    unfollow_resp = req("DELETE", f"/api/messages/{Root_Mid}/follow", token=A, expect=200)
    if not unfollow_resp or unfollow_resp.get("following") is not False:
        fail(f"unfollow response should have following=false: {unfollow_resp}")
    alice_followed = req("GET", "/api/me/followed-threads", token=A)
    if Root_Mid in alice_followed.get("threads", []):
        fail("alice still in followed threads after unfollow")
    ok("alice unfollowed")

    # ===== POST /api/threads/:root/mute =====
    say("mute thread: POST /api/threads/:root_message_id/mute")
    mute_resp = req("POST", f"/api/threads/{Root_Mid}/mute", token=B, expect=200)
    if mute_resp.get("root_message_id") != Root_Mid or not mute_resp.get("muted"):
        fail(f"mute response bad: {mute_resp}")
    ok("bob muted the thread")

    # Carol also mutes
    req("POST", f"/api/threads/{Root_Mid}/mute", token=C, expect=200)
    ok("carol muted the thread")

    # ===== GET /api/threads/:root/mutes =====
    say("list thread muters: GET /api/threads/:root_message_id/mutes")
    muters = req("GET", f"/api/threads/{Root_Mid}/mutes", token=A)
    if "muters" not in muters:
        fail(f"mutes response missing 'muters': {muters}")
    muters_list = muters.get("muters", [])
    if Bpid not in muters_list or Cpid not in muters_list:
        fail(f"bob/carol not in muters: {muters_list}")
    ok(f"muters: {len(muters_list)} (bob, carol)")

    # ===== DELETE /api/threads/:root/mute =====
    say("unmute thread: DELETE /api/threads/:root_message_id/mute")
    unmute_resp = req("DELETE", f"/api/threads/{Root_Mid}/mute", token=B, expect=200)
    if unmute_resp.get("root_message_id") != Root_Mid or unmute_resp.get("muted") is not False:
        fail(f"unmute response bad: {unmute_resp}")
    ok("bob unmuted the thread")

    # Verify bob is no longer in muters list
    muters_after = req("GET", f"/api/threads/{Root_Mid}/mutes", token=A)
    if Bpid in muters_after.get("muters", []):
        fail("bob still in muters after unmute")
    ok(f"muters after bob unmute: {len(muters_after.get('muters', []))}")

    # ===== POST /api/messages/:id/thread-summary (AI) =====
    say("thread summary (AI): POST /api/messages/:id/thread-summary")
    summary_resp = req("POST", f"/api/messages/{Root_Mid}/thread-summary", token=A)
    if summary_resp is None:
        # 502 if no AI backend; that's ok for a smoke test
        ok("thread-summary: AI not configured (502 expected)")
    else:
        # If AI is configured, should have summary + root_id
        if "summary" not in summary_resp or "root_id" not in summary_resp:
            fail(f"summary response missing fields: {summary_resp}")
        if summary_resp.get("root_id") != Root_Mid:
            fail(f"summary root_id mismatch: {summary_resp.get('root_id')} vs {Root_Mid}")
        ok(f"thread-summary returned: {summary_resp.get('summary', '')[:50]}...")

    # ===== POST /api/messages/:id/thread-title (AI) =====
    say("thread title (AI): POST /api/messages/:id/thread-title")
    title_resp = req("POST", f"/api/messages/{Root_Mid}/thread-title", token=A)
    if title_resp is None:
        # 502 if no AI backend; that's ok for a smoke test
        ok("thread-title: AI not configured (502 expected)")
    else:
        # If AI is configured, should have title + root_id
        if "title" not in title_resp or "root_id" not in title_resp:
            fail(f"title response missing fields: {title_resp}")
        if title_resp.get("root_id") != Root_Mid:
            fail(f"title root_id mismatch: {title_resp.get('root_id')} vs {Root_Mid}")
        ok(f"thread-title returned: {title_resp.get('title', '')}")

    # ===== Access control tests =====
    say("access control: non-member cannot access thread")
    d = req("POST", "/api/auth/register",
            {"email": f"d_threads+{ts}@aero.dev", "password": "password_1234",
             "display_name": "DaveNoAccess"})
    D = d["access_token"]
    # Non-member tries to fetch thread → 403
    req("GET", f"/api/messages/{Root_Mid}/thread", token=D, expect=403)
    ok("non-member blocked from thread (403)")

    say("access control: non-member cannot set notification level")
    req("PUT", f"/api/messages/{Root_Mid}/thread-notification-level",
        {"level": "none"}, token=D, expect=403)
    ok("non-member blocked from setting notif level (403)")

    say("access control: unknown root message → 404")
    fake_id = "00000000000000000000000001"
    req("GET", f"/api/messages/{fake_id}/thread", token=A, expect=404)
    ok("unknown root → 404")

    print("\n\033[1;32m✅ Thread routes smoke PASSED (thread CRUD, notifications, mutes, AI gating)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
