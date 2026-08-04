#!/usr/bin/env python3
"""Live smoke for the gap-close batch (f4b1c84): blob-GC reference guard,
per-route rate limiting, and the message.deleted audit event.

The blob-GC test exercises the SHARED-blob reference guard:
  - Upload one blob, reference it from TWO messages (via template-send so the
    File block is delivered over REST).
  - Delete message A  ⇒ blob NOT enqueued (message B still references it).
  - Delete message B  ⇒ blob now enqueued into blob_gc_queue.
A direct psql check on blob_gc_queue confirms the queue state at each step.
"""
import json
import http.client
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = os.environ.get("AERO_BASE", "http://127.0.0.1:8099")
DB = os.environ.get("SMOKE_DB", "aero_rmv2_smoke")
FAILS, OKS = [], []


def req(method, path, token=None, body=None):
    headers = {}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    r = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(r, timeout=10) as resp:
            txt = resp.read().decode()
            return resp.status, (json.loads(txt) if txt else {})
    except urllib.error.HTTPError as e:
        txt = e.read().decode()
        try:
            return e.code, json.loads(txt) if txt else {}
        except Exception:
            return e.code, {"_raw": txt}


def upload(token, room, filename, ctype, data):
    boundary = "----gapclose" + str(int(time.time() * 1000))
    pre = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
           f"filename=\"{filename}\"\r\nContent-Type: {ctype}\r\n\r\n").encode()
    body = pre + data + f"\r\n--{boundary}--\r\n".encode()
    r = urllib.request.Request(BASE + f"/api/rooms/{room}/blobs", data=body, method="POST",
                               headers={"Authorization": "Bearer " + token,
                                        "Content-Type": f"multipart/form-data; boundary={boundary}"})
    with urllib.request.urlopen(r, timeout=10) as resp:
        return resp.status, json.loads(resp.read().decode())


def login_status_from(source_ip, body):
    """Issue one login from a dedicated loopback source address.

    The server keys unauthenticated login throttles by the transport peer, so
    this gives the limiter assertion a fresh, spoof-resistant bucket even when
    other smoke scripts have already exercised login on 127.0.0.1.
    """
    parsed = urllib.parse.urlsplit(BASE)
    connection_type = (
        http.client.HTTPSConnection
        if parsed.scheme == "https"
        else http.client.HTTPConnection
    )
    connection = connection_type(
        parsed.hostname,
        parsed.port,
        timeout=10,
        source_address=(source_ip, 0),
    )
    path = f"{parsed.path.rstrip('/')}/api/auth/login"
    connection.request(
        "POST",
        path,
        body=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    response = connection.getresponse()
    response.read()
    status = response.status
    connection.close()
    return status


def check(name, cond, detail=""):
    (OKS if cond else FAILS).append(name)
    print(f"  [{'OK' if cond else 'FAIL'}] {name}" + (f" — {detail}" if detail and not cond else ""))


def gc_queue_has(blob_id):
    """Query blob_gc_queue directly via docker psql. blob_id is a ULID string;
    the queue stores it as a UUID, so convert by matching on the message side is
    hard — instead count rows whose blob_id (UUID) equals the ULID's uuid form.
    We pass the ULID and let SQL compare against all rows by casting none — simpler:
    just count total rows and check membership by re-deriving the uuid in python."""
    # The queue stores UUIDs; we can't easily map ULID->UUID in shell. Instead
    # count total queue rows (the test controls the DB, starting empty per blob).
    out = subprocess.run(
        ["docker", "exec", "aero-postgres", "psql", "-U", "aero", "-d", DB, "-t", "-A",
         "-c", "SELECT count(*) FROM blob_gc_queue;"],
        capture_output=True, text=True, timeout=10)
    return int(out.stdout.strip() or "0")


sfx = str(int(time.time()))
print("== gap-close smoke ==")

# --- setup ---
st, u = req("POST", "/api/auth/register",
            body={"email": f"gap_{sfx}@x.io", "password": "pw-Aa123456!", "display_name": "gap"})
assert st == 200, f"register: {st} {u}"
tok = u["access_token"]
st, room = req("POST", "/api/rooms", token=tok, body={"name": f"gaproom-{sfx}", "kind": "channel"})
assert st == 200, f"room: {st} {room}"
rid = room["id"]

# === blob-GC reference guard ===
print("-- blob-GC reference guard --")
q0 = gc_queue_has(None)
# Upload one blob.
st, blob = upload(tok, rid, "shared.bin", "application/octet-stream", b"shared-bytes-" + sfx.encode())
check("blob upload 200", st == 200, f"{st} {blob}")
blob_id = blob["id"]

# Create a template carrying a File block referencing the blob, send it TWICE
# (two distinct messages both referencing the same blob).
file_block = {"type": "file", "blob_id": blob_id, "kind": "other", "name": "shared.bin", "size": 13}
st, tmpl = req("POST", "/api/templates", token=tok, body={"name": f"tf-{sfx}", "blocks": [file_block]})
check("template w/ file block 200", st == 200, f"{st} {tmpl}")
tid = tmpl["id"]
st, mA = req("POST", f"/api/templates/{tid}/send", token=tok, body={"room_id": rid})
st2, mB = req("POST", f"/api/templates/{tid}/send", token=tok, body={"room_id": rid})
check("two messages reference the blob", st == 200 and st2 == 200, f"{st}/{st2} {mA} {mB}")
midA, midB = mA.get("id"), mB.get("id")

q_before = gc_queue_has(None)
# Delete message A — blob still referenced by B → NOT enqueued.
st, _ = req("DELETE", f"/api/messages/{midA}", token=tok)
check("delete msg A 204", st == 204, f"{st}")
time.sleep(0.3)
q_after_a = gc_queue_has(None)
check("blob NOT GC-enqueued while still referenced (guard holds)",
      q_after_a == q_before, f"queue {q_before} -> {q_after_a} (expected unchanged)")

# Delete message B — now unreferenced → enqueued.
st, _ = req("DELETE", f"/api/messages/{midB}", token=tok)
check("delete msg B 204", st == 204, f"{st}")
time.sleep(0.3)
q_after_b = gc_queue_has(None)
check("blob GC-enqueued after last reference removed",
      q_after_b == q_after_a + 1, f"queue {q_after_a} -> {q_after_b} (expected +1)")

# === message.deleted audit event ===
print("-- message.deleted audit --")
# Create an owned workspace + a room in it so the audit (resolved by workspace)
# is visible to the owner. The default workspace audit is owner-gated too, but
# the all-zero ws has no admin reader; use an owned ws.
st, ows = req("POST", "/api/workspaces", token=tok, body={"name": f"gapws-{sfx}", "slug": f"gapws{sfx}"})
assert st == 200, f"ws: {st} {ows}"
ows_id = ows["id"]
st, wroom = req("POST", "/api/rooms", token=tok, body={"name": f"war-{sfx}", "kind": "channel", "workspace_id": ows_id})
assert st == 200, f"wroom: {st} {wroom}"
wrid = wroom["id"]
st, tmpl2 = req("POST", "/api/templates", token=tok,
                body={"name": f"td-{sfx}", "blocks": [{"type": "text", "content": "delete me audit digest"}]})
st, msg = req("POST", f"/api/templates/{tmpl2['id']}/send", token=tok, body={"room_id": wrid})
check("audit setup message sent", st == 200, f"{st} {msg}")
st, _ = req("DELETE", f"/api/messages/{msg['id']}", token=tok)
check("delete audited message 204", st == 204, f"{st}")
time.sleep(0.3)
st, audit = req("GET", f"/api/workspaces/{ows_id}/audit", token=tok)
events = audit if isinstance(audit, list) else audit.get("events", audit.get("entries", []))
deleted_evt = next((e for e in events if e.get("event") == "message.deleted" or e.get("action") == "message.deleted"), None)
check("message.deleted audit event present", deleted_evt is not None,
      f"{st} events={[e.get('event') or e.get('action') for e in events][:6]}")
if deleted_evt:
    det = deleted_evt.get("details") or deleted_evt.get("detail") or {}
    if isinstance(det, str):
        try: det = json.loads(det)
        except Exception: det = {}
    check("audit event carries content digest", "digest" in det and det.get("digest"),
          f"details={det}")

# === per-route rate limiting (login 5/min) ===
print("-- per-route rate limit (login 5/min) --")
# Hammer login with WRONG password: each is 401 until the 5/min bucket drains,
# then 429. (burst 5 ⇒ first 5 reach the handler → 401, 6th → 429.)
codes = []
stamp = int(time.time_ns())
source_ip = f"127.{(stamp >> 16) % 250 + 1}.{(stamp >> 8) % 250 + 1}.{stamp % 250 + 1}"
for i in range(8):
    codes.append(login_status_from(
        source_ip,
        {"email": f"gap_{sfx}@x.io", "password": "wrong"},
    ))
got_429 = 429 in codes
check("login rate limit emits 429 after burst", got_429, f"codes={codes}")
# The first few must NOT be 429 (burst allows them through to 401).
check("login burst allows initial attempts (not all 429)", codes[0] != 429, f"codes={codes}")

print()
print(f"== RESULT: {len(OKS)} passed / {len(FAILS)} failed ==")
if FAILS:
    print("FAILED:", ", ".join(FAILS))
    sys.exit(1)
sys.exit(0)
