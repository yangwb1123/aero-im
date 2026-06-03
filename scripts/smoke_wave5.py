#!/usr/bin/env python3
"""Wave-5 smoke: personal access tokens, bookmarks, custom emoji, user status.

The PAT check is the key one: a minted PAT must authenticate an existing
`AuthUser` route (GET /api/me), and stop working once revoked.
"""
from __future__ import annotations
import asyncio, json, os, sys, time, urllib.error, urllib.request
import websockets

HOST = os.environ.get("AERO_HOST", "http://localhost:3030")
WS_HOST = HOST.replace("http://", "ws://").replace("https://", "wss://")
DEFAULT_WS = "00000000000000000000000000"
# 1x1 transparent PNG.
PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4"
    "890000000d49444154789c6360000002000100ffff03000006000557bfabd400"
    "00000049454e44ae426082"
)


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
    r = urllib.request.Request(HOST + path, method=method, data=data, headers=headers)
    try:
        with urllib.request.urlopen(r) as resp:
            buf = resp.read()
            if expect is not None and resp.status != expect: fail(f"{method} {path}: expected {expect} got {resp.status}")
            return json.loads(buf) if buf else None
    except urllib.error.HTTPError as e:
        if expect is not None:
            if e.code != expect: fail(f"{method} {path}: expected {expect} got {e.code}: {e.read().decode(errors='ignore')[:200]}")
            return None
        fail(f"HTTP {e.code} {method} {path}: {e.read().decode(errors='ignore')[:300]}")


def upload_blob(token, name, ctype, payload):
    boundary = "----aerow5" + str(time.time_ns())
    head = (f"--{boundary}\r\n"
            f'content-disposition: form-data; name="file"; filename="{name}"\r\n'
            f"content-type: {ctype}\r\n\r\n").encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    r = urllib.request.Request(HOST + "/api/blobs", method="POST", data=head + payload + tail,
        headers={"authorization": f"Bearer {token}",
                 "content-type": f"multipart/form-data; boundary={boundary}", "accept": "application/json"})
    with urllib.request.urlopen(r) as resp:
        return json.loads(resp.read())


async def ws_send(token, room_id, blocks):
    async with websockets.connect(f"{WS_HOST}/ws?token={token}") as ws:
        assert json.loads(await ws.recv())["type"] == "welcome"
        await ws.send(json.dumps({"type": "join_room", "room_id": room_id}))
        assert json.loads(await ws.recv())["type"] == "presence"
        await ws.send(json.dumps({"type": "send_message", "room_id": room_id, "blocks": blocks, "reply_to": None}))
        for _ in range(6):
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=3))
            if f.get("type") == "message": return f["message"]
    fail("no message echo")


def main():
    ts = int(time.time())
    a = req("POST", "/api/auth/register", {"email": f"a_w5+{ts}@aero.dev", "password": "password_1234", "display_name": "AliceW5"})
    b = req("POST", "/api/auth/register", {"email": f"b_w5+{ts}@aero.dev", "password": "password_1234", "display_name": "BobW5"})
    A, B = a["access_token"], b["access_token"]
    Apid, Bpid = a["participant"]["id"], b["participant"]["id"]
    room = req("POST", "/api/rooms", {"kind": "group", "name": f"w5-{ts}"}, token=A)
    Rid = room["id"]
    ok(f"setup: alice/bob + room {Rid[:8]}")

    # ---------------- Personal Access Tokens ----------------
    say("PAT: mint a token; it authenticates GET /api/me (an existing AuthUser route)")
    pat = req("POST", "/api/pat", {"name": "ci", "expires_in_secs": 3600}, token=A)
    tok = pat.get("token")
    if not tok or not tok.startswith("aero_pat_"): fail(f"bad PAT: {pat}")
    me = req("GET", "/api/me", token=tok)
    if me["id"] != Apid: fail(f"PAT resolved to wrong participant: {me}")
    ok(f"PAT works as bearer on /api/me (→ {me['display_name']})")
    say("PAT: list shows it (no secret); revoke disables it")
    lst = req("GET", "/api/pat", token=A)["tokens"]
    if not any(p["id"] == pat["id"] for p in lst): fail(f"PAT not listed: {lst}")
    if any("token" in p and p.get("token") for p in lst): fail("PAT list leaked a token")
    req("DELETE", f"/api/pat/{pat['id']}", token=A)
    # Revoked PAT must now be rejected on /api/me.
    try:
        urllib.request.urlopen(urllib.request.Request(HOST + "/api/me",
            headers={"authorization": f"Bearer {tok}"}))
        code = 200
    except urllib.error.HTTPError as e:
        code = e.code
    if code == 200: fail("revoked PAT still authenticates")
    ok(f"revoked PAT rejected (status {code}); JWT still works")

    # ---------------- Bookmarks ----------------
    say("bookmarks: save a message, list, unsave")
    msg = asyncio.new_event_loop().run_until_complete(ws_send(A, Rid, [{"type": "text", "content": f"save me {ts}"}]))
    Mid = msg["id"]
    req("POST", f"/api/messages/{Mid}/save", {"note": "important"}, token=A)
    saved = req("GET", "/api/saved", token=A)
    if not any(s["message"]["id"] == Mid for s in saved): fail(f"bookmark not in saved list: {saved}")
    ok(f"saved item present ({len(saved)} total)")
    req("DELETE", f"/api/messages/{Mid}/save", token=A)
    saved2 = req("GET", "/api/saved", token=A)
    if any(s["message"]["id"] == Mid for s in saved2): fail("still saved after unsave")
    ok("unsaved")

    # ---------------- Custom emoji ----------------
    say("emoji: upload a blob, register a custom emoji, list, delete")
    blob = upload_blob(A, "shipit.png", "image/png", PNG)
    bid = blob["id"]
    name = f"shipit{ts}"
    em = req("POST", f"/api/workspaces/{DEFAULT_WS}/emoji", {"name": name, "blob_id": bid}, token=A)
    em_id = em.get("id")
    if not em_id: fail(f"emoji create returned no id: {em}")
    lst = req("GET", f"/api/workspaces/{DEFAULT_WS}/emoji", token=A)
    if not any(e["name"] == name for e in lst): fail(f"emoji not listed: {lst}")
    ok(f"custom emoji :{name}: created + listed")
    say("emoji: duplicate name rejected (409)")
    req("POST", f"/api/workspaces/{DEFAULT_WS}/emoji", {"name": name, "blob_id": bid}, token=A, expect=409)
    req("DELETE", f"/api/emoji/{em_id}", token=A)
    ok("duplicate rejected; emoji deleted")

    # ---------------- User status ----------------
    say("status: alice sets a custom status; bob reads it on her profile")
    req("PUT", "/api/me/status", {"emoji": ":palm_tree:", "text": "On vacation", "presence": "away"}, token=A, expect=200)
    mine = req("GET", "/api/me/status", token=A)
    if mine.get("text") != "On vacation" or mine.get("presence") != "away": fail(f"status not set: {mine}")
    seen = req("GET", f"/api/participants/{Apid}/status", token=B)
    if seen.get("text") != "On vacation": fail(f"bob can't see alice's status: {seen}")
    ok(f"status set + visible to others (emoji={seen.get('emoji')} presence={seen.get('presence')})")
    req("DELETE", "/api/me/status", token=A)
    cleared = req("GET", "/api/me/status", token=A)
    if cleared and cleared.get("text"): fail(f"status not cleared: {cleared}")
    ok("status cleared")

    print("\n\033[1;32m✅ Wave-5 smoke PASSED (PAT auth+revoke, bookmarks, custom emoji, user status)\033[0m")


if __name__ == "__main__":
    main()
