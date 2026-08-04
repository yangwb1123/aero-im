#!/usr/bin/env python3
"""Real two-browser-client smoke test for Aero IM's server-owned SFU.

The script launches one isolated headless Chrome process with fake audio/video,
drives two tabs through the Chrome DevTools Protocol, and imports the repository's
real ``/sfu_calls.js`` controller in each tab.  Both participants must already be
members of ``room_id`` and the Aero server must advertise a browser-reachable SFU
address (for a local run, use ``AERO_SFU_ADVERTISE_HOST=127.0.0.1``).  Pass
``--base-url-b`` to connect participant B to a second gateway and exercise the
cross-node call bridge; omitting it keeps the original single-gateway smoke.

Pass ``--provision`` to register two unique temporary participants, create their
shared group room, and add the second participant before Chrome starts.  Without
that flag, the existing explicit token, participant, and room arguments are used.

Pass ``--same-participant-reconnect`` to open a third tab for participant A on
participant B's gateway, join the same call with a newer durable leg generation,
establish replacement media, and only then close A's superseded WebSocket.  The
replacement A/B RTP counters must continue advancing after stale cleanup settles.

Pass a credentialed ``--turn-url`` with ``--ice-transport-policy relay`` to
force every browser media leg through TURN.  The run succeeds only when Chrome
``getStats()`` reports a selected candidate pair whose local candidate is
``relay``; a merely connected host/srflx pair is rejected.

Only Python's standard library plus the ``websockets`` package is required.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import os
import pathlib
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable

try:
    import websockets
except ModuleNotFoundError:  # Reported after argparse so --help remains usable.
    websockets = None  # type: ignore[assignment]


BOOTSTRAP_JS = r"""
(async () => {
  const cfg = __CONFIG_JSON__;
  const { SfuGroupController } = await import(
    new URL("/sfu_calls.js", cfg.baseUrl).href
  );
  const state = {
    participant: cfg.participant,
    roomId: cfg.roomId,
    callId: null,
    legGeneration: null,
    legGenerationFrames: [],
    welcomeParticipant: null,
    controller: null,
    localStream: null,
    call: null,
    connectionState: "new",
    subscriptions: [],
    answers: 0,
    subscribedAcks: 0,
    topologyFrames: 0,
    iceAcks: 0,
    offers: 0,
    socketClosed: false,
    socketCloseCode: null,
    errors: [],
    frameSummary: [],
    dispatch: Promise.resolve(),
  };
  window.__aeroSfuSmoke = state;

  const endpoint = new URL(cfg.baseUrl);
  endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";
  endpoint.pathname = `${endpoint.pathname.replace(/\/$/, "")}/ws`;
  endpoint.search = new URLSearchParams({ token: cfg.token }).toString();
  endpoint.hash = "";

  const socket = new WebSocket(endpoint.href);
  state.socket = socket;
  const send = (frame) => {
    if (socket.readyState !== WebSocket.OPEN) return false;
    socket.send(JSON.stringify(frame));
    return true;
  };

  socket.addEventListener("message", (message) => {
    let frame;
    try {
      frame = JSON.parse(message.data);
    } catch (error) {
      state.errors.push(`invalid server JSON: ${error}`);
      return;
    }
    const summary = {
      type: frame.type,
      op: frame.event?.op ?? null,
      code: frame.code ?? null,
      call_id: frame.call_id ?? frame.event?.call_id ?? null,
      revision: frame.revision ?? null,
      session_generation: frame.session_generation ?? null,
      leg_generation: frame.event?.leg_generation ?? null,
    };
    state.frameSummary.push(summary);
    if (state.frameSummary.length > 200) state.frameSummary.shift();

    if (frame.type === "welcome") {
      state.welcomeParticipant = frame.participant;
    } else if (
      frame.type === "call"
      && frame.event?.op === "roster"
      && frame.event?.to === cfg.participant
    ) {
      state.callId = frame.event.call_id;
      const generation = Number(frame.event.leg_generation);
      if (Number.isSafeInteger(generation) && generation > 0) {
        state.legGeneration = generation;
        state.legGenerationFrames.push(generation);
      }
    } else if (frame.type === "error") {
      state.errors.push(`${frame.code || "server"}: ${frame.msg || ""}`);
    } else if (frame.type === "call_sfu_answer") {
      state.answers += 1;
    } else if (frame.type === "call_sfu_subscribed") {
      state.subscribedAcks += 1;
    } else if (frame.type === "call_sfu_renegotiate") {
      state.topologyFrames += 1;
    } else if (frame.type === "call_sfu_ice_ack") {
      state.iceAcks += 1;
    }

    state.dispatch = state.dispatch.then(async () => {
      if (!state.controller) return;
      if (frame.type === "call_sfu_answer") {
        await state.controller.handleAnswer(frame);
      } else if (frame.type === "call_sfu_renegotiate") {
        await state.controller.handleTopology(frame);
      } else if (frame.type === "call_sfu_subscribed") {
        state.controller.handleSubscribed(frame);
      }
    }).catch((error) => {
      state.errors.push(`frame dispatch: ${error?.stack || error}`);
    });
  });
  socket.addEventListener("close", (event) => {
    state.socketClosed = true;
    state.socketCloseCode = event.code;
    if (!state.closing) {
      state.errors.push(`websocket closed: ${event.code} ${event.reason || ""}`);
    }
  });

  await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("WebSocket open timed out")),
      cfg.openTimeoutMs,
    );
    socket.addEventListener("open", () => {
      clearTimeout(timer);
      resolve();
    }, { once: true });
    socket.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error("WebSocket open failed"));
    }, { once: true });
  });

  state.join = (callId) => send({
    type: "call_join",
    room_id: cfg.roomId,
    kind: "video",
    call_id: callId || null,
  });

  state.startMedia = async (callId) => {
    state.callId = callId;
    state.localStream = await navigator.mediaDevices.getUserMedia({
      audio: true,
      video: true,
    });
    const kinds = state.localStream.getTracks().map((track) => track.kind);
    if (!kinds.includes("audio") || !kinds.includes("video")) {
      throw new Error(`fake capture did not provide audio+video: ${kinds}`);
    }
    state.call = {
      id: callId,
      roomId: cfg.roomId,
      kind: "video",
      mode: "sfu",
      localStream: state.localStream,
    };
    const adapter = {
      callSfuOffer: (id, room, sdp) => {
        state.offers += 1;
        return send({
          type: "call_sfu_offer",
          call_id: id,
          room_id: room,
          sdp,
        });
      },
      callSfuIce: (id, room, generation, candidate) => send({
        type: "call_sfu_ice",
        call_id: id,
        room_id: room,
        session_generation: generation,
        candidate,
      }),
      callSfuSubscribe: (id, room, generation, revision, tracks) => send({
        type: "call_sfu_subscribe",
        call_id: id,
        room_id: room,
        session_generation: generation,
        revision,
        tracks,
      }),
    };
    state.controller = new SfuGroupController({
      ws: adapter,
      getCall: () => state.call,
      getSelfId: () => cfg.participant,
      // Defaults to host candidates for the ordinary local smoke. A relay
      // acceptance run injects a credentialed TURN server and
      // iceTransportPolicy="relay" through cfg.rtcConfig.
      rtcConfig: () => cfg.rtcConfig || ({ iceServers: [] }),
      onSubscriptions: (subscriptions) => {
        state.subscriptions = subscriptions.map((subscription) => ({
          publisher: String(subscription.publisher || ""),
          pub_mid: String(subscription.pub_mid || ""),
          out_mid: String(subscription.out_mid || ""),
          media_kind: String(
            subscription.media_kind
              || subscription.transceiver?.receiver?.track?.kind
              || "",
          ),
          track_state:
            subscription.transceiver?.receiver?.track?.readyState || null,
        }));
      },
      onConnectionState: (value) => {
        state.connectionState = value;
      },
      onError: (error) => {
        state.errors.push(`controller: ${error?.stack || error}`);
      },
    });
    state.controller.start();
    return kinds;
  };

  state.snapshot = async () => {
    await state.dispatch;
    const media = {
      inbound: {
        audio: { packets: 0, bytes: 0 },
        video: { packets: 0, bytes: 0 },
      },
      outbound: {
        audio: { packets: 0, bytes: 0 },
        video: { packets: 0, bytes: 0 },
      },
      selectedCandidatePairs: 0,
      selectedCandidatePairDetails: [],
    };
    const pc = state.controller?.pc;
    if (pc) {
      const report = await pc.getStats();
      const statsById = new Map();
      const transportSelectedPairIds = new Set();
      report.forEach((stat) => statsById.set(stat.id, stat));
      report.forEach((stat) => {
        if (stat.type === "transport" && stat.selectedCandidatePairId) {
          transportSelectedPairIds.add(stat.selectedCandidatePairId);
        }
      });
      report.forEach((stat) => {
        const kind = String(stat.kind || stat.mediaType || "").toLowerCase();
        if (stat.type === "inbound-rtp" && !stat.isRemote && media.inbound[kind]) {
          media.inbound[kind].packets += Number(stat.packetsReceived || 0);
          media.inbound[kind].bytes += Number(stat.bytesReceived || 0);
        } else if (
          stat.type === "outbound-rtp"
          && !stat.isRemote
          && media.outbound[kind]
        ) {
          media.outbound[kind].packets += Number(stat.packetsSent || 0);
          media.outbound[kind].bytes += Number(stat.bytesSent || 0);
        } else if (
          stat.type === "candidate-pair"
          && stat.state === "succeeded"
          && (
            transportSelectedPairIds.size > 0
              ? transportSelectedPairIds.has(stat.id)
              : (stat.nominated || stat.selected)
          )
        ) {
          media.selectedCandidatePairs += 1;
          const local = statsById.get(stat.localCandidateId);
          const remote = statsById.get(stat.remoteCandidateId);
          media.selectedCandidatePairDetails.push({
            local_candidate_type: local?.candidateType || null,
            remote_candidate_type: remote?.candidateType || null,
            local_protocol: local?.protocol || null,
            remote_protocol: remote?.protocol || null,
            local_address: local?.address || local?.ip || null,
            remote_address: remote?.address || remote?.ip || null,
          });
        }
      });
    }
    return {
      participant: state.participant,
      callId: state.callId,
      legGeneration: state.legGeneration,
      legGenerationFrames: state.legGenerationFrames.slice(),
      welcomeParticipant: state.welcomeParticipant,
      socketReadyState: socket.readyState,
      socketClosed: state.socketClosed,
      socketCloseCode: state.socketCloseCode,
      connectionState: pc?.connectionState || state.connectionState,
      iceConnectionState: pc?.iceConnectionState || null,
      signalingState: pc?.signalingState || null,
      answers: state.answers,
      subscribedAcks: state.subscribedAcks,
      topologyFrames: state.topologyFrames,
      iceAcks: state.iceAcks,
      offers: state.offers,
      subscriptions: state.subscriptions,
      errors: state.errors,
      media,
      recentFrames: state.frameSummary.slice(-30),
    };
  };

  state.closeSocketOnly = async () => {
    state.closing = true;
    if (socket.readyState === WebSocket.CLOSED) return true;
    await new Promise((resolve) => {
      socket.addEventListener("close", resolve, { once: true });
      socket.close(1000, "stale connection close");
    });
    return true;
  };

  state.shutdown = () => {
    state.closing = true;
    if (state.callId) {
      send({
        type: "call_leave",
        call_id: state.callId,
        room_id: cfg.roomId,
      });
    }
    try { state.controller?.close(); } catch (_) {}
    for (const track of state.localStream?.getTracks?.() || []) {
      try { track.stop(); } catch (_) {}
    }
    try { socket.close(1000, "smoke complete"); } catch (_) {}
    return true;
  };

  return { ready: true };
})()
"""


class SmokeFailure(RuntimeError):
    """An expected acceptance/setup failure."""


MAX_PROVISION_RESPONSE_BYTES = 1024 * 1024


def redact_secrets(text: str, values: list[str | None] | tuple[str | None, ...]) -> str:
    """Remove exact in-memory credentials from user-visible diagnostics."""
    redacted = text
    secrets_by_length = sorted(
        {value for value in values if value},
        key=len,
        reverse=True,
    )
    for value in secrets_by_length:
        redacted = redacted.replace(value, "[REDACTED]")
    return redacted


def argument_secrets(args: argparse.Namespace) -> list[str | None]:
    return [
        getattr(args, "token_a", None),
        getattr(args, "token_b", None),
        getattr(args, "turn_credential", None),
    ]


def request_id_suffix(headers: Any) -> str:
    """Return a log-safe request ID suffix without reflecting arbitrary headers."""
    if headers is None:
        return ""
    value = headers.get("x-request-id")
    if not isinstance(value, str) or not value or len(value) > 128:
        return ""
    if not all(character.isalnum() or character in "-_." for character in value):
        return ""
    return f" (request_id={value})"


def provision_api_json(
    base_url: str,
    method: str,
    path: str,
    *,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    timeout: float,
    operation: str,
) -> Any:
    """Call one provisioning endpoint without reflecting credentials or bodies."""
    headers = {"accept": "application/json"}
    data = None
    if token is not None:
        headers["authorization"] = f"Bearer {token}"
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body, separators=(",", ":")).encode()
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}{path}",
        method=method,
        data=data,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = response.read(MAX_PROVISION_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        raise SmokeFailure(
            f"provisioning {operation} failed: HTTP {error.code} {method} {path}"
            f"{request_id_suffix(error.headers)}"
        ) from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        reason = getattr(error, "reason", error)
        raise SmokeFailure(
            f"provisioning {operation} failed: {type(reason).__name__}: {reason}"
        ) from error

    if len(payload) > MAX_PROVISION_RESPONSE_BYTES:
        raise SmokeFailure(
            f"provisioning {operation} failed: response exceeded "
            f"{MAX_PROVISION_RESPONSE_BYTES} bytes"
        )
    if not payload:
        return None
    try:
        return json.loads(payload)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise SmokeFailure(
            f"provisioning {operation} failed: response was not valid JSON"
        ) from error


def required_response_text(document: Any, operation: str, *path: str) -> str:
    """Extract one required string without including the response in failures."""
    value = document
    for key in path:
        if not isinstance(value, dict) or key not in value:
            dotted = ".".join(path)
            raise SmokeFailure(
                f"provisioning {operation} failed: response omitted {dotted}"
            )
        value = value[key]
    if not isinstance(value, str) or not value.strip():
        dotted = ".".join(path)
        raise SmokeFailure(
            f"provisioning {operation} failed: response field {dotted} was invalid"
        )
    return value


def provision_fixture(
    base_url: str,
    timeout: float,
) -> tuple[str, str, str, str, str]:
    """Create a two-participant room fixture and return credentials in memory."""
    unique = f"{time.time_ns():x}-{secrets.token_hex(6)}"
    password_a = secrets.token_urlsafe(32)
    password_b = secrets.token_urlsafe(32)
    sensitive: list[str | None] = [password_a, password_b]

    try:
        participant_a = provision_api_json(
            base_url,
            "POST",
            "/api/auth/register",
            body={
                "email": f"sfu-smoke-a+{unique}@aero.dev",
                "password": password_a,
                "display_name": "SFU Smoke A",
            },
            timeout=timeout,
            operation="register participant A",
        )
        token_a = required_response_text(
            participant_a, "register participant A", "access_token"
        )
        participant_a_id = required_response_text(
            participant_a, "register participant A", "participant", "id"
        )
        sensitive.append(token_a)

        participant_b = provision_api_json(
            base_url,
            "POST",
            "/api/auth/register",
            body={
                "email": f"sfu-smoke-b+{unique}@aero.dev",
                "password": password_b,
                "display_name": "SFU Smoke B",
            },
            timeout=timeout,
            operation="register participant B",
        )
        token_b = required_response_text(
            participant_b, "register participant B", "access_token"
        )
        participant_b_id = required_response_text(
            participant_b, "register participant B", "participant", "id"
        )
        sensitive.append(token_b)

        room = provision_api_json(
            base_url,
            "POST",
            "/api/rooms",
            body={"kind": "group", "name": f"sfu-smoke-{unique}"},
            token=token_a,
            timeout=timeout,
            operation="create shared room",
        )
        room_id = required_response_text(room, "create shared room", "id")
        escaped_room_id = urllib.parse.quote(room_id, safe="")
        provision_api_json(
            base_url,
            "POST",
            f"/api/rooms/{escaped_room_id}/members",
            body={"participant_id": participant_b_id},
            token=token_a,
            timeout=timeout,
            operation="add participant B to shared room",
        )
        return token_a, token_b, participant_a_id, participant_b_id, room_id
    except SmokeFailure as error:
        raise SmokeFailure(redact_secrets(str(error), sensitive)) from error
    except Exception as error:
        raise SmokeFailure(
            f"provisioning failed unexpectedly: {type(error).__name__}"
        ) from error


class CdpPage:
    def __init__(self, target_id: str, websocket_url: str, timeout: float) -> None:
        self.target_id = target_id
        self.websocket_url = websocket_url
        self.timeout = timeout
        self.socket: Any = None
        self.next_id = 1
        self.events: list[dict[str, Any]] = []

    async def connect(self) -> None:
        assert websockets is not None
        self.socket = await websockets.connect(
            self.websocket_url,
            open_timeout=self.timeout,
            close_timeout=2,
            max_size=16 * 1024 * 1024,
        )
        await self.command("Runtime.enable")
        await self.command("Page.enable")

    async def close(self) -> None:
        if self.socket is not None:
            await self.socket.close()
            self.socket = None

    async def command(
        self, method: str, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        if self.socket is None:
            raise SmokeFailure("CDP page is not connected")
        request_id = self.next_id
        self.next_id += 1
        await self.socket.send(
            json.dumps({"id": request_id, "method": method, "params": params or {}})
        )
        while True:
            raw = await asyncio.wait_for(self.socket.recv(), timeout=self.timeout)
            message = json.loads(raw)
            if message.get("id") == request_id:
                if "error" in message:
                    raise SmokeFailure(f"CDP {method}: {message['error']}")
                return message.get("result", {})
            self.events.append(message)
            if len(self.events) > 200:
                self.events.pop(0)

    async def evaluate(self, expression: str) -> Any:
        response = await self.command(
            "Runtime.evaluate",
            {
                "expression": expression,
                "awaitPromise": True,
                "returnByValue": True,
                "userGesture": True,
            },
        )
        if "exceptionDetails" in response:
            details = response["exceptionDetails"]
            description = (
                details.get("exception", {}).get("description")
                or details.get("text")
                or "JavaScript evaluation failed"
            )
            raise SmokeFailure(description)
        result = response.get("result", {})
        if result.get("subtype") == "error":
            raise SmokeFailure(result.get("description", "JavaScript error"))
        return result.get("value")


def http_json(url: str, *, method: str = "GET", timeout: float = 5) -> Any:
    request = urllib.request.Request(url, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


async def wait_for_devtools_port(profile: pathlib.Path, process: subprocess.Popen[Any], timeout: float) -> int:
    marker = profile / "DevToolsActivePort"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SmokeFailure(f"Chrome exited before CDP was ready (exit {process.returncode})")
        try:
            first_line = marker.read_text(encoding="utf-8").splitlines()[0]
            return int(first_line)
        except (FileNotFoundError, IndexError, ValueError):
            await asyncio.sleep(0.1)
    raise SmokeFailure("timed out waiting for Chrome DevToolsActivePort")


async def create_page(port: int, page_url: str, timeout: float) -> CdpPage:
    # Creating a target directly at `page_url` lets Chrome commit navigation
    # while Runtime.enable / Page.enable / Runtime.evaluate are racing on the
    # old execution context. Chrome 150 reports that normal transition as
    # `Execution context was destroyed`. Attach to a stable blank target first,
    # then own the navigation through CDP.
    encoded = urllib.parse.quote("about:blank", safe="")
    endpoint = f"http://127.0.0.1:{port}/json/new?{encoded}"
    try:
        target = await asyncio.to_thread(
            http_json, endpoint, method="PUT", timeout=timeout
        )
    except (urllib.error.URLError, TimeoutError) as error:
        raise SmokeFailure(f"create Chrome tab failed: {error}") from error
    page = CdpPage(target["id"], target["webSocketDebuggerUrl"], timeout)
    try:
        await page.connect()
        navigation = await page.command("Page.navigate", {"url": page_url})
        if navigation.get("errorText"):
            raise SmokeFailure(
                f"browser page navigation failed: {navigation['errorText']}"
            )

        deadline = time.monotonic() + timeout
        requested = urllib.parse.urlsplit(page_url)
        while time.monotonic() < deadline:
            try:
                document = await page.evaluate(
                    "({readyState: document.readyState, href: location.href})"
                )
            except SmokeFailure as error:
                message = str(error).lower()
                if (
                    "execution context was destroyed" not in message
                    and "cannot find context with specified id" not in message
                ):
                    raise
                await asyncio.sleep(0.1)
                continue

            if isinstance(document, dict):
                current = urllib.parse.urlsplit(str(document.get("href", "")))
                reached_requested_origin = (
                    current.scheme == requested.scheme
                    and current.netloc == requested.netloc
                )
                if (
                    reached_requested_origin
                    and document.get("readyState") == "complete"
                ):
                    return page
            await asyncio.sleep(0.1)
        raise SmokeFailure("browser page did not finish loading")
    except Exception:
        await page.close()
        raise


async def wait_for(
    description: str,
    deadline: float,
    probe: Callable[[], Any],
    predicate: Callable[[Any], bool],
) -> Any:
    last: Any = None
    while time.monotonic() < deadline:
        last = await probe()
        if predicate(last):
            return last
        await asyncio.sleep(0.25)
    raise SmokeFailure(
        f"timed out waiting for {description}; last={json.dumps(last, ensure_ascii=False)}"
    )


def media_accepted(snapshot: dict[str, Any], expected_remote: str) -> bool:
    if snapshot.get("connectionState") != "connected":
        return False
    if snapshot.get("answers", 0) < 1 or snapshot.get("subscribedAcks", 0) < 1:
        return False
    if snapshot.get("errors"):
        return False
    subscriptions = {
        (row.get("publisher"), row.get("media_kind"))
        for row in snapshot.get("subscriptions", [])
    }
    if (expected_remote, "audio") not in subscriptions:
        return False
    if (expected_remote, "video") not in subscriptions:
        return False
    media = snapshot.get("media", {})
    for direction in ("inbound", "outbound"):
        for kind in ("audio", "video"):
            counters = media.get(direction, {}).get(kind, {})
            if counters.get("packets", 0) <= 0 or counters.get("bytes", 0) <= 0:
                return False
    return True


def media_progressed(
    before: dict[str, Any],
    after: dict[str, Any],
) -> bool:
    """Require every audio/video RTP counter to keep advancing."""
    before_media = before.get("media", {})
    after_media = after.get("media", {})
    for direction in ("inbound", "outbound"):
        for kind in ("audio", "video"):
            earlier = before_media.get(direction, {}).get(kind, {})
            later = after_media.get(direction, {}).get(kind, {})
            if later.get("packets", 0) <= earlier.get("packets", 0):
                return False
            if later.get("bytes", 0) <= earlier.get("bytes", 0):
                return False
    return True


def selected_local_candidate_is(
    snapshot: dict[str, Any],
    candidate_type: str | None,
) -> bool:
    """Prove the selected browser-side ICE candidate has the required type."""
    if candidate_type is None:
        return True
    details = (
        snapshot.get("media", {}).get("selectedCandidatePairDetails", [])
    )
    return bool(details) and all(
        isinstance(pair, dict)
        and pair.get("local_candidate_type") == candidate_type
        for pair in details
    )


def publisher_accepted(snapshot: dict[str, Any]) -> bool:
    """The first peer has a live server leg before the second publisher joins."""
    if snapshot.get("connectionState") != "connected":
        return False
    if snapshot.get("answers", 0) < 1 or snapshot.get("subscribedAcks", 0) < 1:
        return False
    if snapshot.get("errors"):
        return False
    outbound = snapshot.get("media", {}).get("outbound", {})
    return all(
        outbound.get(kind, {}).get("packets", 0) > 0
        and outbound.get(kind, {}).get("bytes", 0) > 0
        for kind in ("audio", "video")
    )


def chrome_log_tail(path: pathlib.Path, lines: int = 80) -> str:
    try:
        content = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except FileNotFoundError:
        return ""
    return "\n".join(content[-lines:])


def terminate_chrome(process: subprocess.Popen[Any]) -> None:
    if process.poll() is not None:
        return
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGKILL)
    with contextlib.suppress(subprocess.TimeoutExpired):
        process.wait(timeout=3)


async def run_smoke(args: argparse.Namespace) -> None:
    base_url_a = args.base_url.rstrip("/")
    base_url_b = (args.base_url_b or args.base_url).rstrip("/")
    page_url_a = f"{base_url_a}/"
    page_url_b = f"{base_url_b}/"
    ice_servers: list[dict[str, Any]] = []
    if args.turn_url:
        ice_servers.append(
            {
                "urls": args.turn_url,
                "username": args.turn_username,
                "credential": args.turn_credential,
            }
        )
    rtc_config = {
        "iceServers": ice_servers,
        "iceTransportPolicy": args.ice_transport_policy,
    }
    required_local_candidate_type = (
        "relay" if args.ice_transport_policy == "relay" else None
    )

    def accepted(snapshot: dict[str, Any], expected_remote: str) -> bool:
        return media_accepted(
            snapshot, expected_remote
        ) and selected_local_candidate_is(
            snapshot, required_local_candidate_type
        )

    def publisher_ready(snapshot: dict[str, Any]) -> bool:
        return publisher_accepted(snapshot) and selected_local_candidate_is(
            snapshot, required_local_candidate_type
        )
    for option, base_url in (
        ("--base-url", base_url_a),
        ("--base-url-b", base_url_b),
    ):
        parsed = urllib.parse.urlparse(base_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc:
            raise SmokeFailure(f"{option} must be an absolute http(s) URL")

    if args.provision:
        (
            args.token_a,
            args.token_b,
            args.participant_a,
            args.participant_b,
            args.room_id,
        ) = await asyncio.to_thread(
            provision_fixture,
            base_url_a,
            min(args.timeout, 15),
        )

    with tempfile.TemporaryDirectory(prefix="aero-sfu-browser-") as temp:
        temp_path = pathlib.Path(temp)
        profile = temp_path / "chrome-profile"
        chrome_log = temp_path / "chrome.stderr.log"
        profile.mkdir()
        with chrome_log.open("wb") as stderr:
            command = [
                args.chrome,
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--disable-background-timer-throttling",
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-port=0",
                "--remote-allow-origins=*",
                f"--user-data-dir={profile}",
                "--use-fake-device-for-media-stream",
                "--use-fake-ui-for-media-stream",
                "--autoplay-policy=no-user-gesture-required",
                "--disable-features=WebRtcHideLocalIpsWithMdns",
                "about:blank",
            ]
            process = subprocess.Popen(
                command,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
                start_new_session=True,
            )

        pages: list[CdpPage] = []
        snapshots: list[dict[str, Any]] = []
        try:
            port = await wait_for_devtools_port(profile, process, min(args.timeout, 15))
            page_a, page_b = await asyncio.gather(
                create_page(port, page_url_a, min(args.timeout, 15)),
                create_page(port, page_url_b, min(args.timeout, 15)),
            )
            pages = [page_a, page_b]

            configs = [
                {
                    "baseUrl": base_url_a,
                    "token": args.token_a,
                    "participant": args.participant_a,
                    "roomId": args.room_id,
                    "openTimeoutMs": int(min(args.timeout, 15) * 1000),
                    "rtcConfig": rtc_config,
                },
                {
                    "baseUrl": base_url_b,
                    "token": args.token_b,
                    "participant": args.participant_b,
                    "roomId": args.room_id,
                    "openTimeoutMs": int(min(args.timeout, 15) * 1000),
                    "rtcConfig": rtc_config,
                },
            ]
            await asyncio.gather(
                *[
                    page.evaluate(
                        BOOTSTRAP_JS.replace(
                            "__CONFIG_JSON__",
                            json.dumps(config, ensure_ascii=False),
                            1,
                        )
                    )
                    for page, config in zip(pages, configs, strict=True)
                ]
            )

            deadline = time.monotonic() + args.timeout
            await wait_for(
                "both WebSocket welcome frames",
                deadline,
                lambda: asyncio.gather(
                    page_a.evaluate("window.__aeroSfuSmoke.welcomeParticipant"),
                    page_b.evaluate("window.__aeroSfuSmoke.welcomeParticipant"),
                ),
                lambda values: values
                == [args.participant_a, args.participant_b],
            )

            joined = await page_a.evaluate("window.__aeroSfuSmoke.join(null)")
            if not joined:
                raise SmokeFailure("participant A could not send call_join")
            call_id = await wait_for(
                "participant A call roster",
                deadline,
                lambda: page_a.evaluate("window.__aeroSfuSmoke.callId"),
                lambda value: isinstance(value, str) and bool(value),
            )

            joined = await page_b.evaluate(
                f"window.__aeroSfuSmoke.join({json.dumps(call_id)})"
            )
            if not joined:
                raise SmokeFailure("participant B could not send call_join")
            await wait_for(
                "participant B call roster",
                deadline,
                lambda: page_b.evaluate("window.__aeroSfuSmoke.callId"),
                lambda value: value == call_id,
            )

            # Establish A's publisher leg first. This keeps the test
            # deterministic: B's arrival is then the one topology transition
            # that forces A to add receive slots, instead of racing two initial
            # publisher revisions and testing scheduler luck.
            await page_a.evaluate(
                f"window.__aeroSfuSmoke.startMedia({json.dumps(call_id)})"
            )
            await wait_for(
                "participant A initial publisher media",
                deadline,
                lambda: page_a.evaluate("window.__aeroSfuSmoke.snapshot()"),
                publisher_ready,
            )
            await page_b.evaluate(
                f"window.__aeroSfuSmoke.startMedia({json.dumps(call_id)})"
            )

            async def probe_media() -> list[dict[str, Any]]:
                return await asyncio.gather(
                    page_a.evaluate("window.__aeroSfuSmoke.snapshot()"),
                    page_b.evaluate("window.__aeroSfuSmoke.snapshot()"),
                )

            initial_snapshots = await wait_for(
                "bidirectional SFU audio/video media",
                deadline,
                probe_media,
                lambda values: accepted(values[0], args.participant_b)
                and accepted(values[1], args.participant_a),
            )
            await asyncio.sleep(0.75)
            snapshots = await probe_media()
            if not (
                accepted(snapshots[0], args.participant_b)
                and accepted(snapshots[1], args.participant_a)
                and media_progressed(initial_snapshots[0], snapshots[0])
                and media_progressed(initial_snapshots[1], snapshots[1])
            ):
                raise SmokeFailure(
                    "bidirectional audio/video RTP counters stopped advancing"
                )

            result: dict[str, Any] = {
                "ok": True,
                "call_id": call_id,
                "gateway_origins": [base_url_a, base_url_b],
                "ice_transport_policy": args.ice_transport_policy,
                "required_local_candidate_type": required_local_candidate_type,
                "clients": snapshots,
            }
            if args.same_participant_reconnect:
                old_leg_generation = snapshots[0].get("legGeneration")
                if (
                    not isinstance(old_leg_generation, int)
                    or old_leg_generation <= 0
                ):
                    raise SmokeFailure(
                        "participant A initial roster omitted a positive "
                        "leg_generation"
                    )

                # Reconnect A through B's gateway. In a dual-node run this
                # deliberately moves ownership to the other process while the
                # old node-A WebSocket and media leg remain alive.
                page_a_reconnect = await create_page(
                    port,
                    page_url_b,
                    min(args.timeout, 15),
                )
                pages.append(page_a_reconnect)
                reconnect_config = {
                    "baseUrl": base_url_b,
                    "token": args.token_a,
                    "participant": args.participant_a,
                    "roomId": args.room_id,
                    "openTimeoutMs": int(min(args.timeout, 15) * 1000),
                    "rtcConfig": rtc_config,
                }
                await page_a_reconnect.evaluate(
                    BOOTSTRAP_JS.replace(
                        "__CONFIG_JSON__",
                        json.dumps(reconnect_config, ensure_ascii=False),
                        1,
                    )
                )
                await wait_for(
                    "participant A replacement WebSocket welcome",
                    deadline,
                    lambda: page_a_reconnect.evaluate(
                        "window.__aeroSfuSmoke.welcomeParticipant"
                    ),
                    lambda value: value == args.participant_a,
                )
                joined = await page_a_reconnect.evaluate(
                    f"window.__aeroSfuSmoke.join({json.dumps(call_id)})"
                )
                if not joined:
                    raise SmokeFailure(
                        "participant A replacement could not send call_join"
                    )
                replacement_join = await wait_for(
                    "participant A replacement call roster with a newer leg",
                    deadline,
                    lambda: page_a_reconnect.evaluate(
                        "window.__aeroSfuSmoke.snapshot()"
                    ),
                    lambda value: value.get("callId") == call_id
                    and isinstance(value.get("legGeneration"), int)
                    and value["legGeneration"] > old_leg_generation,
                )
                new_leg_generation = replacement_join["legGeneration"]

                await page_a_reconnect.evaluate(
                    f"window.__aeroSfuSmoke.startMedia({json.dumps(call_id)})"
                )

                async def probe_reconnected_media() -> list[dict[str, Any]]:
                    return await asyncio.gather(
                        page_a_reconnect.evaluate(
                            "window.__aeroSfuSmoke.snapshot()"
                        ),
                        page_b.evaluate("window.__aeroSfuSmoke.snapshot()"),
                    )

                replacement_initial = await wait_for(
                    "replacement A and B bidirectional SFU media",
                    deadline,
                    probe_reconnected_media,
                    lambda values: accepted(values[0], args.participant_b)
                    and accepted(values[1], args.participant_a),
                )
                await asyncio.sleep(0.75)
                before_stale_close = await probe_reconnected_media()
                if not (
                    accepted(before_stale_close[0], args.participant_b)
                    and accepted(before_stale_close[1], args.participant_a)
                    and media_progressed(
                        replacement_initial[0], before_stale_close[0]
                    )
                    and media_progressed(
                        replacement_initial[1], before_stale_close[1]
                    )
                ):
                    raise SmokeFailure(
                        "replacement media did not progress before stale close"
                    )

                # Close only the superseded node-A WebSocket. Do not send
                # call_leave: the server's disconnect cleanup must use the
                # generation owned by that old connection and therefore be
                # unable to tear down the replacement leg.
                await page_a.evaluate(
                    "window.__aeroSfuSmoke.closeSocketOnly()"
                )
                await wait_for(
                    "superseded participant A WebSocket close",
                    deadline,
                    lambda: page_a.evaluate(
                        "window.__aeroSfuSmoke.socketClosed"
                    ),
                    bool,
                )

                # Give the old gateway ample time to execute disconnect
                # cleanup, then require every RTP counter on the replacement
                # pair to have advanced across that cleanup boundary.
                await asyncio.sleep(1.5)
                after_stale_cleanup = await probe_reconnected_media()
                if not (
                    accepted(after_stale_cleanup[0], args.participant_b)
                    and accepted(after_stale_cleanup[1], args.participant_a)
                    and media_progressed(
                        before_stale_close[0], after_stale_cleanup[0]
                    )
                    and media_progressed(
                        before_stale_close[1], after_stale_cleanup[1]
                    )
                ):
                    raise SmokeFailure(
                        "stale participant A disconnect stopped replacement "
                        "media"
                    )

                # A second interval guards against sampling packets queued
                # before cleanup rather than a genuinely live replacement.
                await asyncio.sleep(0.75)
                survived = await probe_reconnected_media()
                if not (
                    accepted(survived[0], args.participant_b)
                    and accepted(survived[1], args.participant_a)
                    and media_progressed(after_stale_cleanup[0], survived[0])
                    and media_progressed(after_stale_cleanup[1], survived[1])
                ):
                    raise SmokeFailure(
                        "replacement media did not keep progressing after "
                        "stale cleanup settled"
                    )
                snapshots = survived
                result["clients"] = survived
                result["same_participant_reconnect"] = {
                    "old_gateway_origin": base_url_a,
                    "replacement_gateway_origin": base_url_b,
                    "old_leg_generation": old_leg_generation,
                    "new_leg_generation": new_leg_generation,
                    "stale_socket_closed": True,
                    "media_progressed_during_stale_cleanup": True,
                    "media_progressed_after_stale_cleanup": True,
                }
            print(
                json.dumps(
                    result,
                    ensure_ascii=False,
                    indent=2,
                )
            )
        except Exception:
            if pages:
                with contextlib.suppress(Exception):
                    snapshots = await asyncio.gather(
                        *[
                            page.evaluate(
                                "window.__aeroSfuSmoke?.snapshot?.()"
                                " || {error:'smoke state unavailable'}"
                            )
                            for page in pages
                        ]
                    )
            diagnostics = {
                "ok": False,
                "clients": snapshots,
                "chrome_stderr_tail": chrome_log_tail(chrome_log),
            }
            diagnostic_json = json.dumps(diagnostics, ensure_ascii=False, indent=2)
            print(
                redact_secrets(diagnostic_json, argument_secrets(args)),
                file=sys.stderr,
            )
            raise
        finally:
            if pages:
                await asyncio.gather(
                    *[
                        page.evaluate(
                            "window.__aeroSfuSmoke?.shutdown?.() ?? true"
                        )
                        for page in pages
                    ],
                    return_exceptions=True,
                )
                await asyncio.gather(
                    *[page.close() for page in pages], return_exceptions=True
                )
            terminate_chrome(process)


def environment(name: str) -> str | None:
    value = os.environ.get(name)
    return value if value and value.strip() else None


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Launch two fake-media Chrome tabs and verify Aero IM SFU v2 "
            "ICE/DTLS/SRTP plus bidirectional audio/video RTP."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--base-url",
        default=environment("AERO_SFU_SMOKE_BASE_URL")
        or "http://127.0.0.1:18080",
        help=(
            "participant A gateway origin and provisioning endpoint "
            "(env AERO_SFU_SMOKE_BASE_URL)"
        ),
    )
    parser.add_argument(
        "--base-url-b",
        default=environment("AERO_SFU_SMOKE_BASE_URL_B"),
        help=(
            "optional participant B gateway origin for a cross-node bridge smoke; "
            "defaults to --base-url (env AERO_SFU_SMOKE_BASE_URL_B)"
        ),
    )
    parser.add_argument(
        "--ice-transport-policy",
        choices=("all", "relay"),
        default=environment("AERO_SFU_SMOKE_ICE_TRANSPORT_POLICY") or "all",
        help=(
            "RTCPeerConnection ICE transport policy; relay requires TURN "
            "(env AERO_SFU_SMOKE_ICE_TRANSPORT_POLICY)"
        ),
    )
    parser.add_argument(
        "--turn-url",
        action="append",
        default=None,
        help=(
            "credentialed turn:/turns: URL; repeat for multiple transports "
            "(env AERO_SFU_SMOKE_TURN_URL when omitted)"
        ),
    )
    parser.add_argument(
        "--turn-username",
        default=environment("AERO_SFU_SMOKE_TURN_USERNAME"),
        help="TURN username (env AERO_SFU_SMOKE_TURN_USERNAME)",
    )
    parser.add_argument(
        "--turn-credential",
        default=environment("AERO_SFU_SMOKE_TURN_CREDENTIAL"),
        help="TURN credential (env AERO_SFU_SMOKE_TURN_CREDENTIAL)",
    )
    parser.add_argument(
        "--provision",
        action="store_true",
        help=(
            "register two unique test users and create their shared group room; "
            "generated credentials override explicit fixture arguments"
        ),
    )
    parser.add_argument(
        "--same-participant-reconnect",
        action="store_true",
        help=(
            "after baseline media, reconnect participant A through "
            "--base-url-b with a newer leg_generation, close A's old "
            "WebSocket, and require replacement media to survive"
        ),
    )
    parser.add_argument(
        "--token-a",
        default=argparse.SUPPRESS,
        help="participant A access JWT (env AERO_SFU_SMOKE_TOKEN_A)",
    )
    parser.add_argument(
        "--token-b",
        default=argparse.SUPPRESS,
        help="participant B access JWT (env AERO_SFU_SMOKE_TOKEN_B)",
    )
    parser.add_argument(
        "--participant-a",
        default=argparse.SUPPRESS,
        help="participant A ULID (env AERO_SFU_SMOKE_PARTICIPANT_A)",
    )
    parser.add_argument(
        "--participant-b",
        default=argparse.SUPPRESS,
        help="participant B ULID (env AERO_SFU_SMOKE_PARTICIPANT_B)",
    )
    parser.add_argument(
        "--room-id",
        default=argparse.SUPPRESS,
        help="room ULID shared by both users (env AERO_SFU_SMOKE_ROOM_ID)",
    )
    parser.add_argument(
        "--chrome",
        default=environment("AERO_SFU_SMOKE_CHROME")
        or environment("CHROME_BIN")
        or shutil.which("google-chrome")
        or shutil.which("chromium")
        or "",
        help="Chrome/Chromium executable",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=float(environment("AERO_SFU_SMOKE_TIMEOUT") or "60"),
        help="overall negotiation/media timeout in seconds",
    )
    args = parser.parse_args(argv)
    if args.turn_url is None:
        env_turn_url = environment("AERO_SFU_SMOKE_TURN_URL")
        args.turn_url = [env_turn_url] if env_turn_url else []
    args.turn_url = [url.strip() for url in args.turn_url if url.strip()]
    invalid_turn_urls = [
        url
        for url in args.turn_url
        if (
            urllib.parse.urlparse(url).scheme not in {"turn", "turns"}
            or not (
                urllib.parse.urlparse(url).path
                or urllib.parse.urlparse(url).netloc
            )
            or any(character.isspace() for character in url)
            or "@" in url
        )
    ]
    if invalid_turn_urls:
        parser.error(
            "--turn-url must be a non-empty turn:/turns: URL without "
            "embedded credentials"
        )
    if args.turn_url and (
        not args.turn_username or not args.turn_credential
    ):
        parser.error(
            "--turn-url requires --turn-username and --turn-credential"
        )
    if not args.turn_url and (
        args.turn_username or args.turn_credential
    ):
        parser.error(
            "TURN credentials require at least one --turn-url"
        )
    if args.ice_transport_policy == "relay" and not args.turn_url:
        parser.error(
            "--ice-transport-policy relay requires a credentialed --turn-url"
        )
    for attribute, env_name in (
        ("token_a", "AERO_SFU_SMOKE_TOKEN_A"),
        ("token_b", "AERO_SFU_SMOKE_TOKEN_B"),
        ("participant_a", "AERO_SFU_SMOKE_PARTICIPANT_A"),
        ("participant_b", "AERO_SFU_SMOKE_PARTICIPANT_B"),
        ("room_id", "AERO_SFU_SMOKE_ROOM_ID"),
    ):
        if getattr(args, attribute, None) is None:
            setattr(args, attribute, environment(env_name))

    required_values = [("--chrome", args.chrome)]
    if not args.provision:
        required_values.extend(
            [
                ("--token-a", args.token_a),
                ("--token-b", args.token_b),
                ("--participant-a", args.participant_a),
                ("--participant-b", args.participant_b),
                ("--room-id", args.room_id),
            ]
        )
    missing = [
        option
        for option, value in required_values
        if not value
    ]
    if missing:
        parser.error(f"missing required values: {', '.join(missing)}")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    chrome = shutil.which(args.chrome) if not os.path.isabs(args.chrome) else args.chrome
    if not chrome or not os.path.isfile(chrome) or not os.access(chrome, os.X_OK):
        parser.error(f"Chrome executable is not runnable: {args.chrome}")
    args.chrome = chrome
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if websockets is None:
        print(
            "error: Python package 'websockets' is required for Chrome CDP",
            file=sys.stderr,
        )
        return 2
    try:
        asyncio.run(run_smoke(args))
    except KeyboardInterrupt:
        print("SFU browser smoke interrupted", file=sys.stderr)
        return 130
    except (SmokeFailure, OSError, asyncio.TimeoutError) as error:
        safe_error = redact_secrets(str(error), argument_secrets(args))
        print(f"SFU browser smoke failed: {safe_error}", file=sys.stderr)
        return 1
    except Exception as error:  # Preserve a clean non-zero contract for CI.
        safe_error = redact_secrets(str(error), argument_secrets(args))
        print(f"SFU browser smoke failed unexpectedly: {safe_error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
