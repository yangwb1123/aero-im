#!/usr/bin/env python3
"""Live smoke for async full-data export (ROADMAP 方向四, migration 0070).

Flow: register → send a few messages (via template-send) → POST
/api/me/export/async → poll the job → download the archive blob → assert it
carries the participant, the sent messages, and complete:true. The background
worker runs on a 15s tick, so the poll waits up to ~45s.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("AERO_BASE", "http://127.0.0.1:8099")
FAILS, OKS = [], []


def req(method, path, token=None, body=None, raw=False):
    headers = {}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    r = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(r, timeout=15) as resp:
            body_bytes = resp.read()
            if raw:
                return resp.status, body_bytes
            txt = body_bytes.decode()
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


sfx = str(int(time.time()))
print("== async full-export smoke ==")
st, u = req("POST", "/api/auth/register",
            body={"email": f"axp_{sfx}@x.io", "password": "pw-Aa123456!", "display_name": "axp"})
assert st == 200, f"register: {st} {u}"
tok = u["access_token"]
pid = u["participant"]["id"]
st, room = req("POST", "/api/rooms", token=tok, body={"name": f"axproom-{sfx}", "kind": "channel"})
assert st == 200, f"room: {st}"
rid = room["id"]

# Send 3 messages via template-send so the participant has data to export.
st, tmpl = req("POST", "/api/templates", token=tok,
               body={"name": f"axpt-{sfx}", "blocks": [{"type": "text", "content": f"export me {sfx}"}]})
assert st == 200, f"template: {st} {tmpl}"
for _ in range(3):
    req("POST", f"/api/templates/{tmpl['id']}/send", token=tok, body={"room_id": rid})

# Enqueue async export.
st, job = req("POST", "/api/me/export/async", token=tok)
check("enqueue async export 200 (queued)", st == 200 and job.get("status") == "queued" and job.get("job_id"),
      f"{st} {job}")
job_id = job.get("job_id")

# Cross-user isolation: a second user must NOT see this job (404).
st2, u2 = req("POST", "/api/auth/register",
              body={"email": f"axp2_{sfx}@x.io", "password": "pw-Aa123456!", "display_name": "axp2"})
st, _ = req("GET", f"/api/me/export/jobs/{job_id}", token=u2["access_token"])
check("other user sees job as 404 (no leak)", st == 404, f"{st}")

# Poll our job until done (worker ticks every 15s).
status, download_url = None, None
for i in range(24):  # up to ~48s
    st, j = req("GET", f"/api/me/export/jobs/{job_id}", token=tok)
    status = j.get("status")
    if status == "done":
        download_url = j.get("download_url")
        break
    if status == "failed":
        break
    time.sleep(2)
check("export job reaches done", status == "done", f"final status={status} ({j})")
check("done job has download_url + expires_at", bool(download_url) and j.get("expires_at"),
      f"{j}")

# Download the archive and verify completeness.
if download_url:
    st, body = req("GET", download_url, token=tok, raw=True)
    check("archive download 200", st == 200, f"{st}")
    try:
        archive = json.loads(body.decode())
    except Exception as e:
        archive = {}
        check("archive is valid JSON", False, str(e))
    check("archive marked complete:true", archive.get("complete") is True, f"keys={list(archive.keys())}")
    check("archive carries participant profile", archive.get("participant", {}).get("id") == pid,
          f"participant={archive.get('participant')}")
    msgs = archive.get("messages_sent", [])
    check("archive includes the sent messages (>=3)", len(msgs) >= 3, f"got {len(msgs)} messages")
    check("archive has blobs_uploaded array", isinstance(archive.get("blobs_uploaded"), list),
          f"{type(archive.get('blobs_uploaded'))}")

print()
print(f"== RESULT: {len(OKS)} passed / {len(FAILS)} failed ==")
if FAILS:
    print("FAILED:", ", ".join(FAILS))
    sys.exit(1)
sys.exit(0)
