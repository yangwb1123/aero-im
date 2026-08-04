#!/usr/bin/env python3
"""Live smoke for ROADMAP 第六版 · 方向五·③ — canvas collaborative op log.

Against a running aero-server:
  1. A creates a room + canvas, appends 3 ops → gap-free seqs 1,2,3.
  2. GET /ops returns them ordered, op payload verbatim.
  3. GET /ops?since=2 returns only op 3 (the reconnect catch-up delta).
  4. A non-object op is rejected 400 (bounded, structured ops only).
  5. A non-member is 403 on both append and read (transactional access fence).
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid

B = os.environ.get("AERO_HOST", "http://127.0.0.1:8099")


def call(method, path, tok=None, body=None, expect=None):
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
            st, d = resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        st, d = e.code, e.read().decode()
    if expect is not None and st != expect:
        sys.exit(f"FAIL {method} {path}: got {st} want {expect}: {d}")
    return st, (json.loads(d) if d else {})


def main():
    ts = int(time.time())

    def reg(label):
        _, u = call(
            "POST", "/api/auth/register",
            body={"email": f"{label}_{ts}@aero.dev", "password": "password_123", "display_name": label},
            expect=200,
        )
        return u

    a, b = reg("coA"), reg("coB")
    ta, tb = a["access_token"], b["access_token"]
    _, room = call("POST", "/api/rooms", ta, {"kind": "channel", "name": f"co-{ts}"}, expect=200)
    rid = room["id"]
    _, cv = call("POST", f"/api/rooms/{rid}/canvases", ta, {"title": "Design Doc"}, expect=200)
    cid = cv["id"]

    key1, key2, key3 = (str(uuid.uuid4()) for _ in range(3))
    _, o1 = call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": key1, "op": {"t": "insert", "at": 0, "s": "Hello"}
    }, expect=200)
    _, o2 = call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": key2, "op": {"t": "insert", "at": 5, "s": " World"}
    }, expect=200)
    _, o3 = call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": key3, "op": {"t": "format", "at": 0, "len": 5, "bold": True}
    }, expect=200)
    assert (o1["seq"], o2["seq"], o3["seq"]) == (1, 2, 3), (o1, o2, o3)
    print("✓ appended 3 ops, gap-free seqs 1,2,3")

    _, retry = call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": key1, "op": {"t": "insert", "at": 0, "s": "Hello"}
    }, expect=200)
    assert retry["id"] == o1["id"] and retry["seq"] == 1, retry
    call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": key1, "op": {"t": "insert", "at": 0, "s": "DIFFERENT"}
    }, expect=409)
    print("✓ client_op_id retry returns canonical op; changed payload conflicts")

    _, full = call("GET", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, expect=200)
    assert [o["seq"] for o in full["ops"]] == [1, 2, 3] and full["ops"][0]["op"]["s"] == "Hello"
    print("✓ GET ops returns ordered log, payload verbatim")

    _, delta = call("GET", f"/api/rooms/{rid}/canvases/{cid}/ops?since=2", ta, expect=200)
    assert [o["seq"] for o in delta["ops"]] == [3], delta
    print("✓ ?since=2 returns only op 3 (catch-up delta)")

    _, snap = call("PUT", f"/api/rooms/{rid}/canvases/{cid}", ta, {
        "blocks": [{"type": "text", "content": "materialized through op 3"}],
        "expected_version": cv["version"],
        "snapshot_op_seq": 3,
    }, expect=200)
    assert snap["snapshot_op_seq"] == 3 and snap["version"] == cv["version"] + 1, snap
    _, o4 = call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": str(uuid.uuid4()),
        "op": {"t": "insert", "at": 11, "s": "!"},
    }, expect=200)
    assert o4["seq"] == 4, o4
    call("PUT", f"/api/rooms/{rid}/canvases/{cid}", ta, {
        "blocks": [{"type": "text", "content": "stale snapshot"}],
        "expected_version": snap["version"],
        "snapshot_op_seq": 3,
    }, expect=409)
    _, canonical = call("GET", f"/api/rooms/{rid}/canvases/{cid}", ta, expect=200)
    assert canonical["snapshot_op_seq"] == 3, canonical
    _, tail = call("GET", f"/api/rooms/{rid}/canvases/{cid}/ops?since={canonical['snapshot_op_seq']}", ta, expect=200)
    assert [o["seq"] for o in tail["ops"]] == [4], tail
    print("✓ snapshot baseline is anchored; a racing op stays in the replay tail")

    call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", ta, {
        "client_op_id": str(uuid.uuid4()), "op": "not-an-object"
    }, expect=400)
    print("✓ non-object op rejected 400")

    call("POST", f"/api/rooms/{rid}/canvases/{cid}/ops", tb, {
        "client_op_id": str(uuid.uuid4()), "op": {"t": "insert"}
    }, expect=403)
    call("GET", f"/api/rooms/{rid}/canvases/{cid}/ops", tb, expect=403)
    print("✓ non-member gets 403 on append + read")

    print("RESULT: ALL canvas-op checks passed")


if __name__ == "__main__":
    main()
