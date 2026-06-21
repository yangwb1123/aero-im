#!/usr/bin/env python3
"""Live smoke for the round-4 security fixes (gap-scan #R4-2 MLS authz, #R4-4 WHIP).

  - MLS group state is room-member-only: a non-member's GET is 403, and an
    attacker can't hijack an existing group by upserting it while claiming a room
    they happen to belong to (authorized against the group's CURRENT room).
  - WHIP teardown (DELETE /whip/resource/:key) requires the publisher's stream KEY
    (the secret), not the public stream id — a bogus key is 404, the real key 204.
"""
import base64
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

B = os.environ.get("AERO_HOST", "http://127.0.0.1:8099")


def call(method, path, tok=None, body=None):
    h = {}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        h["content-type"] = "application/json"
    if tok:
        h["authorization"] = f"Bearer {tok}"
    r = urllib.request.Request(B + path, data=data, headers=h, method=method)
    try:
        with urllib.request.urlopen(r, timeout=10) as resp:
            return resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def reg(label):
    _, t = call("POST", "/api/auth/register", body={
        "email": f"{label}_{int(time.time()*1000)}@aero.dev",
        "password": "password_123", "display_name": label})
    return json.loads(t)


def main():
    fails = []
    a, b = reg("r4a"), reg("r4b")
    ta, tb = a["access_token"], b["access_token"]
    rid = json.loads(call("POST", "/api/rooms", ta, {"kind": "group", "name": f"mls-{int(time.time())}"})[1])["id"]
    brid = json.loads(call("POST", "/api/rooms", tb, {"kind": "group", "name": f"bmls-{int(time.time())}"})[1])["id"]

    # Unique per run so reruns against a shared DB don't collide with an existing
    # group (which would correctly 403 the new participant and confuse the test).
    gid_b64 = base64.b64encode(f"group-{time.time_ns()}".encode()).decode()
    body = {"group_id_b64": gid_b64, "ciphersuite": "MLS_128", "epoch": 1,
            "state_b64": base64.b64encode(b"opaque-mls-state").decode(), "room_id": rid}
    if call("POST", "/api/mls/groups", ta, body)[0] != 204:
        fails.append("A (member) upsert should be 204")
    gid_path = urllib.parse.quote(gid_b64, safe="")
    if call("GET", f"/api/mls/groups/{gid_path}", ta)[0] != 200:
        fails.append("A (member) read should be 200")
    if call("GET", f"/api/mls/groups/{gid_path}", tb)[0] != 403:
        fails.append("B (non-member) read should be 403")
    if call("POST", "/api/mls/groups", tb, {**body, "room_id": brid})[0] != 403:
        fails.append("B hijack-upsert (own room) should be 403")
    print("MLS authz checks done")

    st, sresp = call("POST", "/api/streams", ta, {"title": "r4 stream", "room_id": rid, "protocol": "whip"})
    if st not in (200, 201):
        fails.append(f"stream create {st}")
    else:
        key = json.loads(sresp)["ingest_url"].rstrip("/").split("/whip/")[-1]
        if call("DELETE", f"/whip/resource/{'deadbeef'*4}", ta)[0] != 404:
            fails.append("WHIP delete with bogus key should be 404")
        if call("DELETE", f"/whip/resource/{key}", ta)[0] != 204:
            fails.append("WHIP delete with real key should be 204")
    print("WHIP teardown checks done")

    if fails:
        for f in fails:
            print("FAIL:", f)
        sys.exit(1)
    print("RESULT: round-4 security fixes VERIFIED")


if __name__ == "__main__":
    main()
