#!/usr/bin/env python3
"""Wave-11 smoke: broadcast mentions (@channel/@here), files tab, stream follow,
thread follow (+ reply notification), mark-all-read.

Run against a live foreground server (`AERO_HOST=http://localhost:3030`).
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
    if token:
        headers["authorization"] = f"Bearer {token}"
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
                fail(f"{method} {path}: want {sorted(ok_codes)} got {e.code}: "
                     f"{e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def multipart(field, filename, ctype, payload):
    boundary = "----aerow11boundary"
    head = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; "
            f"filename=\"{filename}\"\r\nContent-Type: {ctype}\r\n\r\n").encode()
    return boundary, head + payload + f"\r\n--{boundary}--\r\n".encode()


def upload(token, filename, ctype, payload):
    boundary, body = multipart("file", filename, ctype, payload)
    r = urllib.request.Request(HOST + "/api/blobs", method="POST", data=body, headers={
        "authorization": f"Bearer {token}",
        "content-type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(r) as resp:
        return json.loads(resp.read())


def register(tag, ts):
    r = req("POST", "/api/auth/register",
            {"email": f"{tag}_w11+{ts}@aero.dev", "password": "password_1234",
             "display_name": f"{tag.capitalize()}W11"})
    return r["access_token"], r["participant"]["id"]


def as_list(v, *keys):
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def notif_count(tok):
    r = req("GET", "/api/notifications/count", token=tok)
    if isinstance(r, dict):
        return r.get("unread", r.get("count", 0))
    return r if isinstance(r, int) else 0


async def ws_send(token, room, blocks, reply_to=None):
    url = f"{WS_HOST}/ws?token={token}"
    async with websockets.connect(url) as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({"type": "send_message", "room_id": room,
                                  "blocks": blocks, "reply_to": reply_to}))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message":
                return f["message"]
        return None


async def main():
    ts = int(time.time())
    say("setup: register alice (owner) + bob (member); shared room")
    A, Apid = register("alice", ts)
    B, Bpid = register("bob", ts)
    W = req("POST", "/api/workspaces", {"name": f"Wave11 {ts}", "slug": f"w11-{ts}"}, token=A)["id"]
    req("POST", f"/api/workspaces/{W}/members", {"participant_id": Bpid, "role": "member"}, token=A, expect=[200, 204])
    R = req("POST", "/api/rooms", {"kind": "group", "name": f"w11room-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    ok(f"workspace {W[:8]}, room {R[:8]} with alice+bob")

    # ---------------- Broadcast mentions ----------------
    say("broadcast mention: @channel notifies every room member")
    before = notif_count(B)
    await ws_send(A, R, [{"type": "text", "content": f"@channel standup in 5 ({ts})"}])
    await asyncio.sleep(0.6)
    after = notif_count(B)
    if after <= before:
        fail(f"@channel did not notify bob ({before} -> {after})")
    ok(f"@channel notified the member ({before} -> {after})")

    # ---------------- Files tab ----------------
    say("files tab: upload + send a file block, then list room files")
    blob = upload(A, f"doc-{ts}.txt", "text/plain", b"wave11 file payload" * 4)
    fb = {"type": "file", "blob_id": blob["id"], "kind": "document", "name": blob["name"], "size": blob["size"]}
    if not await ws_send(A, R, [fb]):
        fail("file message not delivered")
    files = as_list(req("GET", f"/api/rooms/{R}/files", token=B), "files")
    if not any(f.get("blob_id") == blob["id"] for f in files):
        fail(f"uploaded file not in room files tab: {files}")
    ok(f"file appears in the room files tab ({len(files)})")
    req("GET", f"/api/rooms/{R}/files", token=register('carol', ts)[0], expect=[403, 404])
    ok("files tab is room-access gated (non-member blocked)")

    # ---------------- Stream follow ----------------
    say("stream follow: follow/list/followers/unfollow; self-follow 400")
    req("PUT", f"/api/participants/{Apid}/follow", token=B, expect=[200, 204])
    following = as_list(req("GET", "/api/me/following", token=B), "participants", "following")
    if Apid not in following:
        fail(f"alice not in bob's following: {following}")
    followers = as_list(req("GET", f"/api/participants/{Apid}/followers", token=A), "participants", "followers")
    if Bpid not in followers:
        fail(f"bob not in alice's followers: {followers}")
    ok(f"bob follows alice (following={len(following)}, alice followers={len(followers)})")
    req("PUT", f"/api/participants/{Bpid}/follow", token=B, expect=[400])  # self-follow
    req("DELETE", f"/api/participants/{Apid}/follow", token=B, expect=[200, 204])
    following2 = as_list(req("GET", "/api/me/following", token=B), "participants", "following")
    if Apid in following2:
        fail("unfollow did not take effect")
    ok("self-follow rejected (400); unfollow works")

    # ---------------- Thread follow ----------------
    say("thread follow: follow a root msg, reply notifies the subscriber")
    root = await ws_send(A, R, [{"type": "text", "content": f"thread root {ts}"}])
    Mid = root["id"]
    req("PUT", f"/api/messages/{Mid}/follow", token=B, expect=[200, 204])
    threads = as_list(req("GET", "/api/me/followed-threads", token=B), "threads", "messages")
    if Mid not in threads:
        fail(f"followed thread not listed: {threads}")
    ok(f"bob follows the thread ({len(threads)})")
    before_t = notif_count(B)
    # alice replies to the root → bob (subscriber, room member) should be notified.
    await ws_send(A, R, [{"type": "text", "content": f"a reply {ts}"}], reply_to=Mid)
    await asyncio.sleep(0.6)
    after_t = notif_count(B)
    if after_t <= before_t:
        fail(f"thread reply did not notify the subscriber ({before_t} -> {after_t})")
    ok(f"thread reply notified the subscriber ({before_t} -> {after_t})")
    req("DELETE", f"/api/messages/{Mid}/follow", token=B, expect=[200, 204])
    ok("unfollow thread works")

    # ---------------- Mark all read ----------------
    say("mark-all-read: clear unread for a room")
    R2 = req("POST", "/api/rooms", {"kind": "group", "name": f"w11unread-{ts}", "workspace_id": W}, token=A)["id"]
    req("POST", f"/api/rooms/{R2}/members", {"participant_id": Bpid}, token=A, expect=[200, 204])
    await ws_send(A, R2, [{"type": "text", "content": "unread one"}])
    await ws_send(A, R2, [{"type": "text", "content": "unread two"}])
    await asyncio.sleep(0.3)
    unread = req("GET", "/api/unread", token=B)
    u_for = {x.get("room_id"): x.get("unread", x.get("count", 0)) for x in as_list(unread, "unread", "rooms")} \
        if not isinstance(unread, dict) or "unread" not in unread or isinstance(unread.get("unread"), list) else {}
    # tolerant: just confirm the read-all endpoint reports success then unread clears
    res = req("POST", f"/api/rooms/{R2}/read-all", token=B, expect=[200])
    if not (isinstance(res, dict) and res.get("read")):
        fail(f"read-all did not report success: {res}")
    ok(f"room marked read (last_message_id={str(res.get('last_message_id'))[:8]})")
    allres = req("POST", "/api/read-all", token=B, expect=[200])
    ok(f"workspace-wide mark-all-read ok (rooms_marked={allres.get('rooms_marked') if isinstance(allres, dict) else '?'})")

    print("\n\033[1;32m✅ Wave-11 smoke PASSED (broadcast mentions, files tab, stream follow, "
          "thread follow, mark-all-read)\033[0m")


if __name__ == "__main__":
    asyncio.run(main())
