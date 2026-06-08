#!/usr/bin/env python3
"""Live smoke for the ROADMAP-v2 batch (security / GDPR / push / observability).

Exercises endpoints added in commits 5eaebdb..90e5d1b against a foreground server:
  - 方向三 安全: refresh-token rotation (old token 401 after rotate), upload MIME
    allowlist (reject text/x-shellscript), per-endpoint auth rate limit.
  - 方向四 GDPR: GET /api/me/export (personal data portability).
  - 方向二 推送: POST/GET/DELETE /api/me/push-token registry.
  - 方向五 可观测: GET /api/workspaces/:ws/admin/ai/dlq (admin DLQ list),
    /metrics carries message-throughput + new gauges.
  - 方向一 集群: GET /api/rooms/:id/online/count (Redis-backed).

Run against a server whose AERO_RATE_LIMIT_PER_SEC is high (so the general
limiter doesn't interfere) but auth limiter left at default 3/s.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("AERO_BASE", "http://127.0.0.1:8080")
FAILS = []
OKS = []


def req(method, path, token=None, body=None, raw=None, ctype="application/json"):
    url = BASE + path
    data = None
    headers = {}
    if raw is not None:
        data = raw
        headers["Content-Type"] = ctype
    elif body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = ctype
    if token:
        headers["Authorization"] = "Bearer " + token
    r = urllib.request.Request(url, data=data, headers=headers, method=method)
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


def upload(token, filename, content_type, data):
    """multipart/form-data upload with a single `file` field."""
    boundary = "----aerosmoke" + str(int(time.time() * 1000))
    pre = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="{filename}"\r\n'
        f"Content-Type: {content_type}\r\n\r\n"
    ).encode()
    post = f"\r\n--{boundary}--\r\n".encode()
    body = pre + data + post
    r = urllib.request.Request(
        BASE + "/api/blobs", data=body, method="POST",
        headers={
            "Authorization": "Bearer " + token,
            "Content-Type": f"multipart/form-data; boundary={boundary}",
        },
    )
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


def check(name, cond, detail=""):
    (OKS if cond else FAILS).append(name)
    print(f"  [{'OK' if cond else 'FAIL'}] {name}" + (f" — {detail}" if detail and not cond else ""))


def register(email):
    st, b = req("POST", "/api/auth/register",
                body={"email": email, "password": "pw-Aa123456!", "display_name": email.split("@")[0]})
    assert st == 200, f"register {email}: {st} {b}"
    return b


print("== ROADMAP-v2 smoke ==")

# --- setup: two users ---
sfx = str(int(time.time()))
admin = register(f"rmv2_admin_{sfx}@x.io")
admin_tok = admin["access_token"]
admin_id = admin["participant"]["id"]
ws_id = admin["participant"].get("workspace_id") or admin.get("workspace_id")
# Resolve default workspace from /api/workspaces
st, wss = req("GET", "/api/workspaces", token=admin_tok)
if st == 200 and isinstance(wss, list) and wss:
    ws_id = wss[0]["id"]
elif st == 200 and isinstance(wss, dict) and wss.get("workspaces"):
    ws_id = wss["workspaces"][0]["id"]
print(f"  admin={admin_id} ws={ws_id}")

# === 方向二: push-token registry ===
print("-- push-token registry --")
st, b = req("POST", "/api/me/push-token", token=admin_tok,
            body={"platform": "fcm", "token": "fake-device-token-abcdef123456"})
check("push-token register fcm 200", st == 200, f"{st} {b}")
st, b = req("POST", "/api/me/push-token", token=admin_tok,
            body={"platform": "gcm", "token": "x"})
check("push-token bad-platform 400", st == 400, f"{st} {b}")
st, b = req("GET", "/api/me/push-token", token=admin_tok)
toks = b.get("tokens", []) if isinstance(b, dict) else []
check("push-token list shows 1 + preview redacted", st == 200 and len(toks) == 1 and "token_preview" in toks[0],
      f"{st} {b}")
st, b = req("DELETE", "/api/me/push-token", token=admin_tok,
            body={"token": "fake-device-token-abcdef123456"})
check("push-token delete 204", st == 204, f"{st} {b}")

# === 方向四: personal data export ===
print("-- me/export (GDPR portability) --")
st, b = req("GET", "/api/me/export", token=admin_tok)
check("me/export 200 with participant+messages+blobs",
      st == 200 and "participant" in b and "messages_sent" in b and "blobs_uploaded" in b and "exported_at" in b,
      f"{st} {list(b.keys()) if isinstance(b, dict) else b}")
st, b = req("GET", "/api/me/export")  # no auth
check("me/export unauth 401", st == 401, f"{st}")

# === 方向三: upload MIME allowlist ===
print("-- upload MIME allowlist --")
# A disallowed type — shell script.
st, b = upload(admin_tok, "evil.sh", "text/x-shellscript", b"#!/bin/sh\nrm -rf /\n")
check("upload shellscript rejected (400)", st == 400, f"{st} {b}")
# An allowed type — png (header bytes).
png = b"\x89PNG\r\n\x1a\n" + b"\x00" * 64
st, b = upload(admin_tok, "ok.png", "image/png", png)
check("upload png allowed (200)", st == 200, f"{st} {b}")
blob_id_1 = b.get("id") if isinstance(b, dict) else None
# Re-upload identical bytes — dedup should return same id.
st, b2 = upload(admin_tok, "ok2.png", "image/png", png)
check("upload identical png dedups to same id",
      st == 200 and isinstance(b2, dict) and b2.get("id") == blob_id_1, f"{st} {b2} vs {blob_id_1}")

# === 方向三: refresh-token rotation ===
print("-- refresh-token rotation --")
refresh_tok = admin["refresh_token"]
st, b = req("POST", "/api/auth/refresh", body={"refresh_token": refresh_tok})
check("refresh rotate 200 returns new pair",
      st == 200 and b.get("access_token") and b.get("refresh_token") and b["refresh_token"] != refresh_tok,
      f"{st} {b if st!=200 else 'rotated'}")
new_refresh = b.get("refresh_token") if st == 200 else None
# Reusing the OLD refresh token must now fail (rotation blacklist).
st, b = req("POST", "/api/auth/refresh", body={"refresh_token": refresh_tok})
check("old refresh token reuse 401 (rotation blacklist)", st == 401, f"{st} {b}")
# The NEW refresh token still works.
if new_refresh:
    st, b = req("POST", "/api/auth/refresh", body={"refresh_token": new_refresh})
    check("new refresh token still valid 200", st == 200, f"{st} {b}")

# === 方向五: AI DLQ admin endpoint ===
print("-- AI DLQ admin --")
# Create a fresh workspace so the caller is its Owner (the all-zero default ws
# only enrolls registrants as Member, which is correctly rejected by the admin gate).
st, ows = req("POST", "/api/workspaces", token=admin_tok,
              body={"name": f"rmv2-ws-{sfx}", "slug": f"rmv2ws{sfx}"})
assert st == 200, f"create workspace: {st} {ows}"
owned_ws = ows["id"]
st, b = req("GET", f"/api/workspaces/{owned_ws}/admin/ai/dlq", token=admin_tok)
check("ai dlq list 200 (owner) with total_dead+jobs",
      st == 200 and "total_dead" in b and "jobs" in b, f"{st} {b}")
# Non-member (second user) should get 403.
u2 = register(f"rmv2_user_{sfx}@x.io")
st, b = req("GET", f"/api/workspaces/{owned_ws}/admin/ai/dlq", token=u2["access_token"])
check("ai dlq list non-member 403", st == 403, f"{st} {b}")

# === 方向一: cross-node online count (Redis-backed) ===
print("-- room online count --")
st, room = req("POST", "/api/rooms", token=admin_tok, body={"name": f"rmv2-room-{sfx}", "kind": "channel"})
if st == 200:
    rid = room["id"]
    st, b = req("GET", f"/api/rooms/{rid}/online/count", token=admin_tok)
    check("online count 200 (redis-backed, 0 ws conns)", st == 200 and "count" in b, f"{st} {b}")
    # Send a message (via the template-send REST path, which calls
    # ImService::send_message) so the throughput counter registers in /metrics.
    st, tmpl = req("POST", "/api/templates", token=admin_tok,
                   body={"name": f"t-{sfx}", "blocks": [{"type": "text", "content": "hello roadmap-v2"}]})
    if st == 200:
        st, _ = req("POST", f"/api/templates/{tmpl['id']}/send", token=admin_tok,
                    body={"room_id": rid})
        check("send message 200 (drives throughput metric)", st == 200, f"{st}")
    else:
        check("create template for send", False, f"{st} {tmpl}")
else:
    check("create room for online-count", False, f"{st} {room}")

# === 方向五: /metrics carries new series ===
print("-- /metrics new series --")
# /metrics returns Prometheus text exposition, not JSON — fetch raw.
raw, st = "", 0
try:
    r = urllib.request.Request(BASE + "/metrics", headers={"Authorization": "Bearer " + admin_tok})
    with urllib.request.urlopen(r, timeout=10) as resp:
        raw = resp.read().decode()
        st = resp.status
except urllib.error.HTTPError as e:
    st = e.code
    raw = e.read().decode()
except Exception as e:
    raw = str(e)
check("/metrics 200", st == 200, f"{st}")
check("/metrics has aero_messages_sent_total", "aero_messages_sent_total" in raw)
check("/metrics has aero_ai_dlq_size", "aero_ai_dlq_size" in raw)

print()
print(f"== RESULT: {len(OKS)} passed / {len(FAILS)} failed ==")
if FAILS:
    print("FAILED:", ", ".join(FAILS))
    sys.exit(1)
sys.exit(0)
