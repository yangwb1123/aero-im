#!/usr/bin/env python3
"""Real two-session Firefox smoke for Aero IM's server-owned SFU.

The harness uses only Python's standard library and the W3C WebDriver HTTP
protocol.  It starts two isolated geckodriver/Firefox sessions with fake camera
and microphone devices, imports the repository's real ``/sfu_calls.js`` module,
and verifies ICE/DTLS/SRTP plus continuously advancing bidirectional audio/video
RTP counters.

Pass ``--provision`` to create unique temporary participants and a shared room.
Otherwise provide explicit token, participant, and room fixture arguments.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import pathlib
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable


BOOTSTRAP_JS = r"""
const cfg = arguments[0];
const done = arguments[arguments.length - 1];

(async () => {
  const summarizeSdp = (sdp) => String(sdp || "")
    .split(/\r?\n/)
    .filter((line) => (
      line.startsWith("m=")
      || line.startsWith("a=mid:")
      || /^a=(sendrecv|sendonly|recvonly|inactive)$/.test(line)
      || line.startsWith("a=rtpmap:")
    ));
  const { SfuGroupController } = await import(
    new URL("/sfu_calls.js", cfg.baseUrl).href
  );
  const state = {
    participant: cfg.participant,
    roomId: cfg.roomId,
    callId: null,
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
    emptyCandidatesSkipped: 0,
    localCandidates: [],
    answerCandidates: [],
    lastOfferSdp: null,
    lastAnswerSdp: null,
    errors: [],
    frameSummary: [],
    dispatch: Promise.resolve(),
    closing: false,
  };
  window.__aeroFirefoxSmoke = state;

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
    state.frameSummary.push({
      type: frame.type,
      op: frame.event?.op ?? null,
      code: frame.code ?? null,
      call_id: frame.call_id ?? frame.event?.call_id ?? null,
      revision: frame.revision ?? null,
      session_generation: frame.session_generation ?? null,
    });
    if (state.frameSummary.length > 100) state.frameSummary.shift();

    if (frame.type === "welcome") {
      state.welcomeParticipant = frame.participant;
    } else if (
      frame.type === "call"
      && frame.event?.op === "roster"
      && frame.event?.to === cfg.participant
    ) {
      state.callId = frame.event.call_id;
    } else if (frame.type === "error") {
      state.errors.push(`${frame.code || "server"}: ${frame.msg || ""}`);
    } else if (frame.type === "call_sfu_answer") {
      state.answers += 1;
      state.lastAnswerSdp = String(frame.sdp || "");
      state.answerCandidates = String(frame.sdp || "")
        .split(/\r?\n/)
        .filter((line) => line.startsWith("a=candidate:"))
        .map((line) => line.slice(2));
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
      video: { width: 640, height: 480, frameRate: 15 },
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
        state.lastOfferSdp = String(sdp || "");
        return send({
          type: "call_sfu_offer",
          call_id: id,
          room_id: room,
          sdp,
        });
      },
      callSfuIce: (id, room, generation, candidate) => {
        const candidateSdp = String(candidate?.candidate || "").trim();
        if (!candidateSdp) {
          state.emptyCandidatesSkipped += 1;
          return true;
        }
        if (state.localCandidates.length < 50) {
          state.localCandidates.push(candidateSdp);
        }
        return send({
          type: "call_sfu_ice",
          call_id: id,
          room_id: room,
          session_generation: generation,
          candidate: { ...candidate, candidate: candidateSdp },
        });
      },
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
      rtcConfig: () => ({ iceServers: [] }),
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
        state.errors.push(
          `controller: ${error?.name || "Error"}: ${error?.message || error}`
          + `\n${error?.stack || ""}`,
        );
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
      rtp: [],
    };
    const pc = state.controller?.pc;
    if (pc) {
      const report = await pc.getStats();
      report.forEach((stat) => {
        const kind = String(stat.kind || stat.mediaType || "").toLowerCase();
        if (
          stat.type === "inbound-rtp"
          || stat.type === "outbound-rtp"
          || stat.type === "remote-inbound-rtp"
          || stat.type === "remote-outbound-rtp"
        ) {
          const codec = report.get(stat.codecId);
          media.rtp.push({
            type: stat.type,
            kind,
            mid: stat.mid ?? null,
            codec: codec?.mimeType || null,
            payloadType: codec?.payloadType ?? null,
            packetsReceived: Number(stat.packetsReceived || 0),
            packetsSent: Number(stat.packetsSent || 0),
            packetsLost: Number(stat.packetsLost || 0),
            bytesReceived: Number(stat.bytesReceived || 0),
            bytesSent: Number(stat.bytesSent || 0),
            framesEncoded: Number(stat.framesEncoded || 0),
            framesDecoded: Number(stat.framesDecoded || 0),
            keyFramesEncoded: Number(stat.keyFramesEncoded || 0),
            keyFramesDecoded: Number(stat.keyFramesDecoded || 0),
          });
        }
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
          && (stat.nominated || stat.selected)
        ) {
          media.selectedCandidatePairs += 1;
        }
      });
    }
    return {
      participant: state.participant,
      callId: state.callId,
      welcomeParticipant: state.welcomeParticipant,
      connectionState: pc?.connectionState || state.connectionState,
      iceConnectionState: pc?.iceConnectionState || null,
      iceGatheringState: pc?.iceGatheringState || null,
      signalingState: pc?.signalingState || null,
      answers: state.answers,
      subscribedAcks: state.subscribedAcks,
      topologyFrames: state.topologyFrames,
      iceAcks: state.iceAcks,
      offers: state.offers,
      emptyCandidatesSkipped: state.emptyCandidatesSkipped,
      localCandidates: state.localCandidates.slice(),
      answerCandidates: state.answerCandidates.slice(),
      subscriptions: state.subscriptions,
      errors: state.errors,
      media,
      transceivers: (pc?.getTransceivers?.() || []).map((transceiver) => ({
        mid: transceiver.mid,
        direction: transceiver.direction,
        currentDirection: transceiver.currentDirection,
        senderKind: transceiver.sender?.track?.kind || null,
        senderState: transceiver.sender?.track?.readyState || null,
        receiverKind: transceiver.receiver?.track?.kind || null,
        receiverState: transceiver.receiver?.track?.readyState || null,
      })),
      offerMedia: summarizeSdp(state.lastOfferSdp),
      answerMedia: summarizeSdp(state.lastAnswerSdp),
      recentFrames: state.frameSummary.slice(-30),
    };
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
    try { socket.close(1000, "firefox smoke complete"); } catch (_) {}
    return true;
  };

  return { ok: true };
})().then(
  (value) => done(value),
  (error) => done({ ok: false, error: String(error?.stack || error) }),
);
"""


CALL_METHOD_JS = r"""
const method = arguments[0];
const params = arguments[1];
const done = arguments[arguments.length - 1];
const state = window.__aeroFirefoxSmoke;
if (!state || typeof state[method] !== "function") {
  done({ ok: false, error: `unknown smoke method: ${method}` });
} else {
  Promise.resolve(state[method](...params)).then(
    (value) => done({ ok: true, value }),
    (error) => done({ ok: false, error: String(error?.stack || error) }),
  );
}
"""


class SmokeFailure(RuntimeError):
    """An expected acceptance or setup failure."""


MAX_HTTP_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_PROVISION_RESPONSE_BYTES = 1024 * 1024


def redact_secrets(text: str, values: list[str | None]) -> str:
    redacted = text
    for value in sorted({value for value in values if value}, key=len, reverse=True):
        redacted = redacted.replace(value, "[REDACTED]")
    return redacted


def argument_secrets(args: argparse.Namespace) -> list[str | None]:
    return [args.token_a, args.token_b]


def request_json(
    url: str,
    *,
    method: str = "GET",
    body: Any = None,
    headers: dict[str, str] | None = None,
    timeout: float,
    max_bytes: int = MAX_HTTP_RESPONSE_BYTES,
) -> Any:
    encoded = None
    request_headers = dict(headers or {})
    if body is not None:
        encoded = json.dumps(body, separators=(",", ":")).encode()
        request_headers["content-type"] = "application/json"
    request = urllib.request.Request(
        url,
        data=encoded,
        headers=request_headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = response.read(max_bytes + 1)
    except urllib.error.HTTPError as error:
        payload = error.read(max_bytes + 1)
        detail = ""
        with contextlib.suppress(json.JSONDecodeError, UnicodeDecodeError):
            document = json.loads(payload)
            value = document.get("value", document)
            if isinstance(value, dict):
                message = value.get("message") or value.get("error")
                if isinstance(message, str):
                    detail = f": {message}"
        raise SmokeFailure(f"HTTP {error.code} {method} {url}{detail}") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise SmokeFailure(f"{method} {url} failed: {error}") from error
    if len(payload) > max_bytes:
        raise SmokeFailure(f"{method} {url} response exceeded {max_bytes} bytes")
    if not payload:
        return None
    try:
        return json.loads(payload)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise SmokeFailure(f"{method} {url} returned invalid JSON") from error


def required_text(document: Any, operation: str, *path: str) -> str:
    value = document
    for key in path:
        if not isinstance(value, dict) or key not in value:
            raise SmokeFailure(
                f"{operation} response omitted {'.'.join(path)}"
            )
        value = value[key]
    if not isinstance(value, str) or not value.strip():
        raise SmokeFailure(
            f"{operation} response field {'.'.join(path)} was invalid"
        )
    return value


def provision_api(
    base_url: str,
    method: str,
    path: str,
    *,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    timeout: float,
) -> Any:
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    return request_json(
        f"{base_url.rstrip('/')}{path}",
        method=method,
        body=body,
        headers=headers,
        timeout=timeout,
        max_bytes=MAX_PROVISION_RESPONSE_BYTES,
    )


def provision_fixture(
    base_url: str,
    timeout: float,
) -> tuple[str, str, str, str, str]:
    unique = f"{time.time_ns():x}-{secrets.token_hex(6)}"
    password_a = secrets.token_urlsafe(32)
    password_b = secrets.token_urlsafe(32)
    sensitive: list[str | None] = [password_a, password_b]
    try:
        a = provision_api(
            base_url,
            "POST",
            "/api/auth/register",
            body={
                "email": f"firefox-sfu-a+{unique}@aero.dev",
                "password": password_a,
                "display_name": "Firefox SFU A",
            },
            timeout=timeout,
        )
        token_a = required_text(a, "register A", "access_token")
        participant_a = required_text(a, "register A", "participant", "id")
        sensitive.append(token_a)

        b = provision_api(
            base_url,
            "POST",
            "/api/auth/register",
            body={
                "email": f"firefox-sfu-b+{unique}@aero.dev",
                "password": password_b,
                "display_name": "Firefox SFU B",
            },
            timeout=timeout,
        )
        token_b = required_text(b, "register B", "access_token")
        participant_b = required_text(b, "register B", "participant", "id")
        sensitive.append(token_b)

        room = provision_api(
            base_url,
            "POST",
            "/api/rooms",
            body={"kind": "group", "name": f"firefox-sfu-{unique}"},
            token=token_a,
            timeout=timeout,
        )
        room_id = required_text(room, "create room", "id")
        provision_api(
            base_url,
            "POST",
            f"/api/rooms/{urllib.parse.quote(room_id, safe='')}/members",
            body={"participant_id": participant_b},
            token=token_a,
            timeout=timeout,
        )
        return token_a, token_b, participant_a, participant_b, room_id
    except Exception as error:
        safe = redact_secrets(str(error), sensitive)
        raise SmokeFailure(f"fixture provisioning failed: {safe}") from error


def unused_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def log_tail(path: pathlib.Path, lines: int = 100) -> str:
    with contextlib.suppress(FileNotFoundError):
        return "\n".join(
            path.read_text(encoding="utf-8", errors="replace").splitlines()[-lines:]
        )
    return ""


class FirefoxSession:
    """One geckodriver process and its single Firefox WebDriver session."""

    def __init__(
        self,
        name: str,
        *,
        geckodriver: str,
        firefox: str | None,
        temp_dir: pathlib.Path,
        timeout: float,
    ) -> None:
        self.name = name
        self.geckodriver = geckodriver
        self.firefox = firefox
        self.temp_dir = temp_dir
        self.timeout = timeout
        self.port = unused_loopback_port()
        self.process: subprocess.Popen[Any] | None = None
        self.session_id: str | None = None
        self.log_path = temp_dir / f"geckodriver-{name}.log"
        self._log_handle: Any = None

    @property
    def endpoint(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> None:
        environment = os.environ.copy()
        environment["TMPDIR"] = str(self.temp_dir)
        self._log_handle = self.log_path.open("wb")
        self.process = subprocess.Popen(
            [
                self.geckodriver,
                "--host",
                "127.0.0.1",
                "--port",
                str(self.port),
                "--log",
                "info",
            ],
            stdout=self._log_handle,
            stderr=subprocess.STDOUT,
            env=environment,
            start_new_session=True,
        )
        deadline = time.monotonic() + min(self.timeout, 20)
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise SmokeFailure(
                    f"geckodriver {self.name} exited with {self.process.returncode}"
                )
            try:
                status = request_json(
                    f"{self.endpoint}/status",
                    timeout=1,
                )
                if status.get("value", {}).get("ready"):
                    break
            except SmokeFailure:
                pass
            time.sleep(0.1)
        else:
            raise SmokeFailure(f"geckodriver {self.name} did not become ready")

        firefox_options: dict[str, Any] = {
            "args": ["-headless"],
            "prefs": {
                "media.navigator.streams.fake": True,
                "media.navigator.permission.disabled": True,
                "media.autoplay.default": 0,
                "media.autoplay.blocking_policy": 0,
                "media.autoplay.block-webaudio": False,
                "media.peerconnection.ice.obfuscate_host_addresses": False,
            },
        }
        if self.firefox:
            firefox_options["binary"] = self.firefox
        created = self._command(
            "/session",
            method="POST",
            body={
                "capabilities": {
                    "alwaysMatch": {
                        "browserName": "firefox",
                        "acceptInsecureCerts": True,
                        "pageLoadStrategy": "normal",
                        "moz:firefoxOptions": firefox_options,
                    }
                }
            },
            include_session=False,
            timeout=min(self.timeout, 40),
        )
        value = created.get("value", {})
        self.session_id = value.get("sessionId")
        if not isinstance(self.session_id, str) or not self.session_id:
            raise SmokeFailure(f"geckodriver {self.name} omitted sessionId")
        self._command(
            "/timeouts",
            method="POST",
            body={
                "script": int(self.timeout * 1000),
                "pageLoad": int(min(self.timeout, 30) * 1000),
                "implicit": 0,
            },
        )

    def _command(
        self,
        path: str,
        *,
        method: str = "GET",
        body: Any = None,
        include_session: bool = True,
        timeout: float | None = None,
    ) -> Any:
        if include_session:
            if not self.session_id:
                raise SmokeFailure(f"Firefox {self.name} has no active session")
            path = f"/session/{self.session_id}{path}"
        return request_json(
            f"{self.endpoint}{path}",
            method=method,
            body=body,
            timeout=timeout or min(self.timeout, 30),
        )

    def navigate(self, url: str) -> None:
        self._command("/url", method="POST", body={"url": url})
        ready = self.execute_sync("return document.readyState;", [])
        if ready != "complete":
            raise SmokeFailure(f"Firefox {self.name} page was not complete")

    def execute_sync(self, script: str, args: list[Any]) -> Any:
        response = self._command(
            "/execute/sync",
            method="POST",
            body={"script": script, "args": args},
        )
        return response.get("value")

    def execute_async(self, script: str, args: list[Any]) -> Any:
        response = self._command(
            "/execute/async",
            method="POST",
            body={"script": script, "args": args},
            timeout=self.timeout + 5,
        )
        return response.get("value")

    def bootstrap(self, config: dict[str, Any]) -> None:
        result = self.execute_async(BOOTSTRAP_JS, [config])
        if not isinstance(result, dict) or not result.get("ok"):
            message = result.get("error") if isinstance(result, dict) else result
            raise SmokeFailure(f"Firefox {self.name} bootstrap failed: {message}")

    def call(self, method: str, *params: Any) -> Any:
        result = self.execute_async(CALL_METHOD_JS, [method, list(params)])
        if not isinstance(result, dict) or not result.get("ok"):
            message = result.get("error") if isinstance(result, dict) else result
            raise SmokeFailure(
                f"Firefox {self.name} smoke method {method} failed: {message}"
            )
        return result.get("value")

    def close(self) -> None:
        if self.session_id:
            with contextlib.suppress(Exception):
                self.call("shutdown")
            with contextlib.suppress(Exception):
                self._command("", method="DELETE", timeout=10)
            self.session_id = None
        if self.process and self.process.poll() is None:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.process.pid, signal.SIGKILL)
                with contextlib.suppress(subprocess.TimeoutExpired):
                    self.process.wait(timeout=3)
        if self._log_handle is not None:
            self._log_handle.close()
            self._log_handle = None


def wait_for(
    description: str,
    deadline: float,
    probe: Callable[[], Any],
    predicate: Callable[[Any], bool],
) -> Any:
    last: Any = None
    while time.monotonic() < deadline:
        last = probe()
        if predicate(last):
            return last
        values = last if isinstance(last, list) else [last]
        for value in values:
            if not isinstance(value, dict):
                continue
            errors = value.get("errors")
            if errors or value.get("connectionState") == "failed":
                raise SmokeFailure(
                    f"{description} reached terminal browser state; "
                    f"last={json.dumps(last, ensure_ascii=False)}"
                )
        time.sleep(0.25)
    raise SmokeFailure(
        f"timed out waiting for {description}; "
        f"last={json.dumps(last, ensure_ascii=False)}"
    )


def publisher_accepted(snapshot: dict[str, Any]) -> bool:
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


def media_accepted(snapshot: dict[str, Any], expected_remote: str) -> bool:
    if snapshot.get("connectionState") != "connected":
        return False
    if snapshot.get("iceConnectionState") not in {"connected", "completed"}:
        return False
    if snapshot.get("answers", 0) < 1 or snapshot.get("subscribedAcks", 0) < 1:
        return False
    if snapshot.get("errors"):
        return False
    subscriptions = {
        (
            row.get("publisher"),
            row.get("media_kind"),
            row.get("track_state"),
        )
        for row in snapshot.get("subscriptions", [])
    }
    if (expected_remote, "audio", "live") not in subscriptions:
        return False
    if (expected_remote, "video", "live") not in subscriptions:
        return False
    media = snapshot.get("media", {})
    for direction in ("inbound", "outbound"):
        for kind in ("audio", "video"):
            counters = media.get(direction, {}).get(kind, {})
            if counters.get("packets", 0) <= 0 or counters.get("bytes", 0) <= 0:
                return False
    return True


def media_progressed(before: dict[str, Any], after: dict[str, Any]) -> bool:
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


def run_smoke(args: argparse.Namespace) -> None:
    base_url = args.base_url.rstrip("/")
    parsed = urllib.parse.urlparse(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise SmokeFailure("--base-url must be an absolute http(s) URL")
    if args.provision:
        (
            args.token_a,
            args.token_b,
            args.participant_a,
            args.participant_b,
            args.room_id,
        ) = provision_fixture(base_url, min(args.timeout, 15))

    with tempfile.TemporaryDirectory(prefix="aero-firefox-sfu-") as temp:
        temp_path = pathlib.Path(temp)
        sessions = [
            FirefoxSession(
                "a",
                geckodriver=args.geckodriver,
                firefox=args.firefox,
                temp_dir=temp_path,
                timeout=args.timeout,
            ),
            FirefoxSession(
                "b",
                geckodriver=args.geckodriver,
                firefox=args.firefox,
                temp_dir=temp_path,
                timeout=args.timeout,
            ),
        ]
        snapshots: list[dict[str, Any]] = []
        try:
            for session in sessions:
                session.start()
                session.navigate(f"{base_url}/")
            configs = [
                {
                    "baseUrl": base_url,
                    "token": args.token_a,
                    "participant": args.participant_a,
                    "roomId": args.room_id,
                    "openTimeoutMs": int(min(args.timeout, 15) * 1000),
                },
                {
                    "baseUrl": base_url,
                    "token": args.token_b,
                    "participant": args.participant_b,
                    "roomId": args.room_id,
                    "openTimeoutMs": int(min(args.timeout, 15) * 1000),
                },
            ]
            for session, config in zip(sessions, configs, strict=True):
                session.bootstrap(config)

            a, b = sessions
            deadline = time.monotonic() + args.timeout
            wait_for(
                "both Firefox WebSocket welcome frames",
                deadline,
                lambda: [
                    a.call("snapshot").get("welcomeParticipant"),
                    b.call("snapshot").get("welcomeParticipant"),
                ],
                lambda values: values
                == [args.participant_a, args.participant_b],
            )

            if not a.call("join", None):
                raise SmokeFailure("Firefox A could not send call_join")
            call_id = wait_for(
                "Firefox A call roster",
                deadline,
                lambda: a.call("snapshot").get("callId"),
                lambda value: isinstance(value, str) and bool(value),
            )
            if not b.call("join", call_id):
                raise SmokeFailure("Firefox B could not send call_join")
            wait_for(
                "Firefox B call roster",
                deadline,
                lambda: b.call("snapshot").get("callId"),
                lambda value: value == call_id,
            )

            a.call("startMedia", call_id)
            wait_for(
                "Firefox A initial publisher media",
                deadline,
                lambda: a.call("snapshot"),
                publisher_accepted,
            )
            b.call("startMedia", call_id)

            def probe_media() -> list[dict[str, Any]]:
                return [a.call("snapshot"), b.call("snapshot")]

            initial = wait_for(
                "Firefox bidirectional SFU audio/video media",
                deadline,
                probe_media,
                lambda values: media_accepted(values[0], args.participant_b)
                and media_accepted(values[1], args.participant_a),
            )
            time.sleep(1.25)
            snapshots = probe_media()
            if not (
                media_accepted(snapshots[0], args.participant_b)
                and media_accepted(snapshots[1], args.participant_a)
                and media_progressed(initial[0], snapshots[0])
                and media_progressed(initial[1], snapshots[1])
            ):
                raise SmokeFailure(
                    "Firefox bidirectional audio/video counters stopped advancing"
                )
            print(
                json.dumps(
                    {
                        "ok": True,
                        "browser": "firefox",
                        "call_id": call_id,
                        "gateway_origin": base_url,
                        "clients": snapshots,
                    },
                    ensure_ascii=False,
                    indent=2,
                )
            )
        except Exception:
            with contextlib.suppress(Exception):
                snapshots = [
                    session.call("snapshot")
                    for session in sessions
                    if session.session_id
                ]
            diagnostics = {
                "ok": False,
                "clients": snapshots,
                "geckodriver_logs": {
                    session.name: log_tail(session.log_path)
                    for session in sessions
                },
            }
            print(
                redact_secrets(
                    json.dumps(diagnostics, ensure_ascii=False, indent=2),
                    argument_secrets(args),
                ),
                file=sys.stderr,
            )
            raise
        finally:
            for session in reversed(sessions):
                session.close()


def environment(name: str) -> str | None:
    value = os.environ.get(name)
    return value if value and value.strip() else None


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Launch two isolated fake-media Firefox sessions and verify Aero IM "
            "SFU ICE/DTLS/SRTP plus bidirectional audio/video RTP."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--base-url",
        default=environment("AERO_FIREFOX_SFU_BASE_URL")
        or "http://127.0.0.1:18085",
        help="Aero gateway origin (env AERO_FIREFOX_SFU_BASE_URL)",
    )
    parser.add_argument(
        "--provision",
        action="store_true",
        help="create two unique participants and their shared group room",
    )
    parser.add_argument("--token-a", default=argparse.SUPPRESS)
    parser.add_argument("--token-b", default=argparse.SUPPRESS)
    parser.add_argument("--participant-a", default=argparse.SUPPRESS)
    parser.add_argument("--participant-b", default=argparse.SUPPRESS)
    parser.add_argument("--room-id", default=argparse.SUPPRESS)
    parser.add_argument(
        "--firefox",
        default=environment("FIREFOX_BIN"),
        help=(
            "real Firefox binary; omitted lets geckodriver discover Snap "
            "Firefox (env FIREFOX_BIN)"
        ),
    )
    parser.add_argument(
        "--geckodriver",
        default=environment("GECKODRIVER_BIN")
        or shutil.which("geckodriver")
        or "/snap/bin/geckodriver",
        help="geckodriver executable",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=float(environment("AERO_FIREFOX_SFU_TIMEOUT") or "90"),
        help="overall negotiation/media timeout in seconds",
    )
    args = parser.parse_args(argv)
    for attribute, env_name in (
        ("token_a", "AERO_FIREFOX_SFU_TOKEN_A"),
        ("token_b", "AERO_FIREFOX_SFU_TOKEN_B"),
        ("participant_a", "AERO_FIREFOX_SFU_PARTICIPANT_A"),
        ("participant_b", "AERO_FIREFOX_SFU_PARTICIPANT_B"),
        ("room_id", "AERO_FIREFOX_SFU_ROOM_ID"),
    ):
        if getattr(args, attribute, None) is None:
            setattr(args, attribute, environment(env_name))
    if not args.provision:
        missing = [
            option
            for option, value in (
                ("--token-a", args.token_a),
                ("--token-b", args.token_b),
                ("--participant-a", args.participant_a),
                ("--participant-b", args.participant_b),
                ("--room-id", args.room_id),
            )
            if not value
        ]
        if missing:
            parser.error(f"missing required values: {', '.join(missing)}")
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    executables = [("--geckodriver", args.geckodriver)]
    if args.firefox:
        executables.append(("--firefox", args.firefox))
    for option, executable in executables:
        resolved = (
            shutil.which(executable)
            if not os.path.isabs(executable)
            else executable
        )
        if not resolved or not os.path.isfile(resolved) or not os.access(
            resolved, os.X_OK
        ):
            parser.error(f"{option} executable is not runnable: {executable}")
        if option == "--firefox":
            args.firefox = resolved
        else:
            args.geckodriver = resolved
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        run_smoke(args)
    except KeyboardInterrupt:
        print("Firefox SFU smoke interrupted", file=sys.stderr)
        return 130
    except (SmokeFailure, OSError, TimeoutError) as error:
        safe = redact_secrets(str(error), argument_secrets(args))
        print(f"Firefox SFU smoke failed: {safe}", file=sys.stderr)
        return 1
    except Exception as error:
        safe = redact_secrets(str(error), argument_secrets(args))
        print(f"Firefox SFU smoke failed unexpectedly: {safe}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
