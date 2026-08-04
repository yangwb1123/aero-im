#!/usr/bin/env python3
"""Real headless-Chrome playback smoke for Aero IM's WHIP -> WHEP path.

The script provisions a temporary account and WHIP stream, publishes an H.264
test pattern with FFmpeg, then drives Google Chrome through the Chrome DevTools
Protocol. Chrome performs the production WHEP SDP exchange and must prove:

* ICE + DTLS/SRTP reach a connected state;
* the selected candidate pair succeeded;
* a live, unmuted remote video track was delivered;
* the negotiated inbound codec is H.264;
* RTP packet/byte counters keep advancing; and
* at least one video frame was decoded.

It expects a running Aero server backed by Postgres, Redis and NATS. For a
localhost server, advertise ``AERO_INGEST_HOST=127.0.0.1`` so Chrome can reach
the UDP candidates in both the WHIP and WHEP SDP answers.

Python's standard library plus the ``websockets`` package are required. Stream
keys, generated passwords and access tokens are kept in memory and redacted
from publisher diagnostics.
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
except ModuleNotFoundError:
    websockets = None  # type: ignore[assignment]


BOOTSTRAP_JS = r"""
(async () => {
  const cfg = __CONFIG_JSON__;
  const state = {
    answerHasH264: false,
    errors: [],
    http: null,
    offeredH264Codecs: 0,
    remoteTrack: null,
    resourceUrl: null,
    shutdownPromise: null,
    trackEvents: 0,
  };
  window.__aeroWhepSmoke = state;

  const video = document.createElement("video");
  video.id = "aero-whep-smoke-video";
  video.autoplay = true;
  video.controls = false;
  video.muted = true;
  video.playsInline = true;
  document.body.replaceChildren(video);
  state.video = video;

  const pc = new RTCPeerConnection({ iceServers: [] });
  state.pc = pc;
  const transceiver = pc.addTransceiver("video", { direction: "recvonly" });
  state.transceiver = transceiver;

  const capabilities = RTCRtpReceiver.getCapabilities("video");
  const h264 = (capabilities?.codecs || []).filter(
    (codec) => codec.mimeType.toLowerCase() === "video/h264",
  );
  const packetized = h264.filter(
    (codec) => /(?:^|;)packetization-mode=1(?:;|$)/i.test(
      codec.sdpFmtpLine || "",
    ),
  );
  const preferred = packetized.length ? packetized : h264;
  if (!preferred.length) {
    throw new Error("Chrome exposes no H.264 receive codec");
  }
  transceiver.setCodecPreferences(preferred);
  state.offeredH264Codecs = preferred.length;

  pc.addEventListener("track", (event) => {
    state.trackEvents += 1;
    state.remoteTrack = event.track;
    const stream = event.streams[0] || new MediaStream([event.track]);
    video.srcObject = stream;
    video.play().catch((error) => {
      state.errors.push(`video.play: ${error?.message || error}`);
    });
  });

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  if (pc.iceGatheringState !== "complete") {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pc.removeEventListener("icegatheringstatechange", onState);
        reject(new Error("Chrome ICE gathering timed out"));
      }, cfg.signalTimeoutMs);
      const onState = () => {
        if (pc.iceGatheringState === "complete") {
          clearTimeout(timer);
          pc.removeEventListener("icegatheringstatechange", onState);
          resolve();
        }
      };
      pc.addEventListener("icegatheringstatechange", onState);
    });
  }

  const response = await fetch(
    new URL(`/whep/${encodeURIComponent(cfg.streamId)}`, cfg.baseUrl),
    {
      method: "POST",
      headers: {
        accept: "application/sdp",
        "content-type": "application/sdp",
      },
      body: pc.localDescription.sdp,
      redirect: "follow",
    },
  );
  const answerSdp = await response.text();
  state.http = {
    contentType: response.headers.get("content-type"),
    redirected: response.redirected,
    status: response.status,
  };
  if (response.status !== 201) {
    throw new Error(
      `WHEP POST returned HTTP ${response.status}: ${answerSdp.slice(0, 200)}`,
    );
  }
  state.answerHasH264 = /a=rtpmap:\d+\s+H264\/90000/im.test(answerSdp);
  if (!state.answerHasH264) {
    throw new Error("WHEP SDP answer did not negotiate H.264");
  }
  const location = response.headers.get("location");
  state.resourceUrl = location ? new URL(location, response.url).href : null;
  if (!state.resourceUrl) {
    throw new Error("WHEP response omitted its resource Location");
  }
  await pc.setRemoteDescription({ type: "answer", sdp: answerSdp });

  state.snapshot = async () => {
    const reports = await pc.getStats();
    const inboundRows = [];
    for (const report of reports.values()) {
      if (
        report.type === "inbound-rtp"
        && !report.isRemote
        && (report.kind === "video" || report.mediaType === "video")
      ) {
        inboundRows.push(report);
      }
    }
    inboundRows.sort(
      (left, right) => (right.bytesReceived || 0) - (left.bytesReceived || 0),
    );
    const inbound = inboundRows[0] || null;
    const codec = inbound?.codecId ? reports.get(inbound.codecId) : null;

    let pair = null;
    const transport = Array.from(reports.values()).find(
      (report) => report.type === "transport" && report.selectedCandidatePairId,
    );
    if (transport) pair = reports.get(transport.selectedCandidatePairId) || null;
    if (!pair) {
      pair = Array.from(reports.values()).find(
        (report) => report.type === "candidate-pair"
          && report.state === "succeeded"
          && (report.nominated || report.selected),
      ) || null;
    }
    const local = pair?.localCandidateId
      ? reports.get(pair.localCandidateId)
      : null;
    const remote = pair?.remoteCandidateId
      ? reports.get(pair.remoteCandidateId)
      : null;
    const track = state.remoteTrack;

    return {
      answerHasH264: state.answerHasH264,
      connectionState: pc.connectionState,
      errors: state.errors.slice(),
      http: state.http,
      iceConnectionState: pc.iceConnectionState,
      iceGatheringState: pc.iceGatheringState,
      inbound: {
        bytesReceived: inbound?.bytesReceived || 0,
        codecFmtp: codec?.sdpFmtpLine || null,
        codecMimeType: codec?.mimeType || null,
        framesDecoded: inbound?.framesDecoded || 0,
        framesDropped: inbound?.framesDropped || 0,
        framesReceived: inbound?.framesReceived || 0,
        keyFramesDecoded: inbound?.keyFramesDecoded || 0,
        packetsLost: inbound?.packetsLost || 0,
        packetsReceived: inbound?.packetsReceived || 0,
      },
      offeredH264Codecs: state.offeredH264Codecs,
      resourcePresent: Boolean(state.resourceUrl),
      selectedCandidatePair: pair ? {
        localCandidateType: local?.candidateType || null,
        localProtocol: local?.protocol || null,
        nominated: Boolean(pair.nominated),
        remoteCandidateType: remote?.candidateType || null,
        remoteProtocol: remote?.protocol || null,
        state: pair.state || null,
      } : null,
      signalingState: pc.signalingState,
      track: {
        events: state.trackEvents,
        muted: track?.muted ?? null,
        readyState: track?.readyState || null,
      },
      video: {
        currentTime: video.currentTime || 0,
        height: video.videoHeight || 0,
        readyState: video.readyState,
        width: video.videoWidth || 0,
      },
    };
  };

  state.shutdown = () => {
    if (!state.shutdownPromise) {
      state.shutdownPromise = (async () => {
        let deleteStatus = null;
        if (state.resourceUrl) {
          try {
            const deleted = await fetch(state.resourceUrl, { method: "DELETE" });
            deleteStatus = deleted.status;
          } catch (error) {
            state.errors.push(`WHEP DELETE: ${error?.message || error}`);
          }
        }
        try { pc.close(); } catch (_) {}
        try { state.remoteTrack?.stop(); } catch (_) {}
        return { deleteStatus };
      })();
    }
    return state.shutdownPromise;
  };

  return { ready: true };
})()
"""


class SmokeFailure(RuntimeError):
    """An expected staging or acceptance failure."""


class CdpPage:
    """Minimal sequential Chrome DevTools Protocol client."""

    def __init__(self, websocket_url: str, timeout: float) -> None:
        self.websocket_url = websocket_url
        self.timeout = timeout
        self.socket: Any = None
        self.next_id = 1

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
        self,
        method: str,
        params: dict[str, Any] | None = None,
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
            if message.get("id") != request_id:
                continue
            if "error" in message:
                raise SmokeFailure(f"CDP {method}: {message['error']}")
            return message.get("result", {})

    async def evaluate(self, expression: str) -> Any:
        response = await self.command(
            "Runtime.evaluate",
            {
                "awaitPromise": True,
                "expression": expression,
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


def api_json(
    base_url: str,
    method: str,
    path: str,
    *,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    accepted: tuple[int, ...] = (200,),
    timeout: float = 15,
) -> Any:
    headers = {"accept": "application/json"}
    payload = None
    if token:
        headers["authorization"] = f"Bearer {token}"
    if body is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(body, separators=(",", ":")).encode()
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}{path}",
        method=method,
        data=payload,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            status = response.status
            raw = response.read(1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        status = error.code
        raw = error.read(1024 * 1024 + 1)
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        reason = getattr(error, "reason", error)
        raise SmokeFailure(
            f"{method} {path} failed: {type(reason).__name__}: {reason}"
        ) from error
    if status not in accepted:
        raise SmokeFailure(f"{method} {path} returned HTTP {status}")
    if len(raw) > 1024 * 1024:
        raise SmokeFailure(f"{method} {path} response exceeded 1 MiB")
    if not raw:
        return None
    try:
        return json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise SmokeFailure(f"{method} {path} returned invalid JSON") from error


def required_text(document: Any, operation: str, *path: str) -> str:
    value = document
    for key in path:
        if not isinstance(value, dict) or key not in value:
            raise SmokeFailure(f"{operation} response omitted {'.'.join(path)}")
        value = value[key]
    if not isinstance(value, str) or not value.strip():
        raise SmokeFailure(f"{operation} response field {'.'.join(path)} was invalid")
    return value


def provision_fixture(base_url: str, timeout: float) -> tuple[str, str, str, str]:
    unique = f"{time.time_ns():x}-{secrets.token_hex(5)}"
    password = secrets.token_urlsafe(32)
    registered = api_json(
        base_url,
        "POST",
        "/api/auth/register",
        body={
            "display_name": "WHEP Browser Smoke",
            "email": f"whep-smoke+{unique}@aero.dev",
            "password": password,
        },
        timeout=timeout,
    )
    token = required_text(registered, "register", "access_token")
    stream = api_json(
        base_url,
        "POST",
        "/api/streams",
        body={"protocol": "whip", "title": f"whep-smoke-{unique}"},
        token=token,
        timeout=timeout,
    )
    stream_id = required_text(stream, "create stream", "id")
    stream_key = required_text(stream, "create stream", "stream_key")
    ingest_url = required_text(stream, "create stream", "ingest_url")
    parsed = urllib.parse.urlsplit(ingest_url)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname or not parsed.port:
        raise SmokeFailure("create stream returned an invalid WHIP ingest URL")
    return token, stream_id, stream_key, ingest_url


def publisher_command(ffmpeg: str, ingest_url: str) -> list[str]:
    return [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-tune",
        "zerolatency",
        "-profile:v",
        "baseline",
        "-level:v",
        "3.1",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "30",
        "-keyint_min",
        "30",
        "-sc_threshold",
        "0",
        "-bf",
        "0",
        "-b:v",
        "700k",
        "-an",
        "-f",
        "whip",
        ingest_url,
    ]


def redact(text: str, secrets_to_hide: tuple[str, ...]) -> str:
    redacted = text
    for value in sorted(set(secrets_to_hide), key=len, reverse=True):
        if value:
            redacted = redacted.replace(value, "[REDACTED]")
    return redacted


def stop_process(process: subprocess.Popen[str] | None) -> str:
    if process is None:
        return ""
    if process.poll() is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
    try:
        _, stderr = process.communicate(timeout=5)
        return stderr or ""
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        with contextlib.suppress(subprocess.TimeoutExpired):
            _, stderr = process.communicate(timeout=3)
            return stderr or ""
    return ""


def terminate_chrome(process: subprocess.Popen[Any] | None) -> None:
    if process is None or process.poll() is not None:
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


def http_json(url: str, *, method: str = "GET", timeout: float = 5) -> Any:
    request = urllib.request.Request(url, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.load(response)


async def wait_for_devtools_port(
    profile: pathlib.Path,
    process: subprocess.Popen[Any],
    timeout: float,
) -> int:
    marker = profile / "DevToolsActivePort"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SmokeFailure(
                f"Chrome exited before CDP was ready (exit {process.returncode})"
            )
        try:
            return int(marker.read_text(encoding="utf-8").splitlines()[0])
        except (FileNotFoundError, IndexError, ValueError):
            await asyncio.sleep(0.1)
    raise SmokeFailure("timed out waiting for Chrome DevToolsActivePort")


async def create_page(port: int, page_url: str, timeout: float) -> CdpPage:
    encoded = urllib.parse.quote("about:blank", safe="")
    try:
        target = await asyncio.to_thread(
            http_json,
            f"http://127.0.0.1:{port}/json/new?{encoded}",
            method="PUT",
            timeout=timeout,
        )
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise SmokeFailure(f"create Chrome tab failed: {error}") from error
    page = CdpPage(target["webSocketDebuggerUrl"], timeout)
    try:
        await page.connect()
        navigation = await page.command("Page.navigate", {"url": page_url})
        if navigation.get("errorText"):
            raise SmokeFailure(f"browser navigation failed: {navigation['errorText']}")
        deadline = time.monotonic() + timeout
        requested = urllib.parse.urlsplit(page_url)
        while time.monotonic() < deadline:
            try:
                document = await page.evaluate(
                    "({readyState:document.readyState,href:location.href})"
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
                if (
                    current.scheme == requested.scheme
                    and current.netloc == requested.netloc
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
        f"timed out waiting for {description}; "
        f"last={json.dumps(last, ensure_ascii=False)}"
    )


async def wait_for_publisher(
    args: argparse.Namespace,
    token: str,
    stream_id: str,
    process: subprocess.Popen[str],
    secrets_to_hide: tuple[str, ...],
) -> None:
    deadline = time.monotonic() + args.timeout
    last_status = "unknown"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            _, stderr = process.communicate(timeout=2)
            tail = redact((stderr or "")[-1500:], secrets_to_hide)
            raise SmokeFailure(
                f"FFmpeg WHIP publisher exited {process.returncode}: {tail}"
            )
        stream = await asyncio.to_thread(
            api_json,
            args.base_url,
            "GET",
            f"/api/streams/{stream_id}",
            token=token,
            timeout=min(args.timeout, 15),
        )
        last_status = str(stream.get("status", "unknown"))
        if last_status.lower() == "live":
            await asyncio.sleep(0.5)
            return
        await asyncio.sleep(0.25)
    raise SmokeFailure(
        f"WHIP publisher did not mark stream live within {args.timeout:.1f}s "
        f"(last status: {last_status})"
    )


def playback_ready(snapshot: Any) -> bool:
    if not isinstance(snapshot, dict) or snapshot.get("errors"):
        return False
    if snapshot.get("connectionState") != "connected":
        return False
    if snapshot.get("iceConnectionState") not in {"connected", "completed"}:
        return False
    if snapshot.get("http", {}).get("status") != 201:
        return False
    if not snapshot.get("answerHasH264") or not snapshot.get("resourcePresent"):
        return False
    pair = snapshot.get("selectedCandidatePair") or {}
    if pair.get("state") != "succeeded":
        return False
    track = snapshot.get("track") or {}
    if track.get("events", 0) < 1:
        return False
    if track.get("readyState") != "live" or track.get("muted") is not False:
        return False
    inbound = snapshot.get("inbound") or {}
    if str(inbound.get("codecMimeType", "")).lower() != "video/h264":
        return False
    if inbound.get("packetsReceived", 0) < 10:
        return False
    if inbound.get("bytesReceived", 0) < 5000:
        return False
    video = snapshot.get("video") or {}
    return (
        inbound.get("framesDecoded", 0) > 0
        or (video.get("width", 0) > 0 and video.get("height", 0) > 0)
    )


def safe_result(
    stream_id: str,
    before: dict[str, Any],
    after: dict[str, Any],
    delete_status: int | None,
) -> dict[str, Any]:
    earlier = before["inbound"]
    later = after["inbound"]
    return {
        "ok": True,
        "stream_id": stream_id,
        "whep": {
            "answer_h264": after["answerHasH264"],
            "content_type": after["http"]["contentType"],
            "delete_status": delete_status,
            "http_status": after["http"]["status"],
            "resource_present": after["resourcePresent"],
        },
        "transport": {
            "connection_state": after["connectionState"],
            "ice_connection_state": after["iceConnectionState"],
            "selected_candidate_pair": after["selectedCandidatePair"],
        },
        "track": after["track"],
        "video": after["video"],
        "inbound_video": {
            **later,
            "bytes_delta": later["bytesReceived"] - earlier["bytesReceived"],
            "packets_delta": later["packetsReceived"] - earlier["packetsReceived"],
        },
    }


async def run_smoke(args: argparse.Namespace) -> None:
    token, stream_id, stream_key, ingest_url = await asyncio.to_thread(
        provision_fixture,
        args.base_url,
        min(args.timeout, 15),
    )
    secrets_to_hide = (token, stream_key, ingest_url)
    publisher: subprocess.Popen[str] | None = subprocess.Popen(
        publisher_command(args.ffmpeg, ingest_url),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    chrome: subprocess.Popen[Any] | None = None
    page: CdpPage | None = None
    chrome_log_path: pathlib.Path | None = None
    last_snapshot: Any = None

    try:
        await wait_for_publisher(
            args,
            token,
            stream_id,
            publisher,
            secrets_to_hide,
        )
        with tempfile.TemporaryDirectory(prefix="aero-whep-browser-") as temp:
            temp_path = pathlib.Path(temp)
            profile = temp_path / "chrome-profile"
            profile.mkdir()
            chrome_log_path = temp_path / "chrome.stderr.log"
            with chrome_log_path.open("wb") as stderr:
                chrome = subprocess.Popen(
                    [
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
                        "--autoplay-policy=no-user-gesture-required",
                        "--disable-features=WebRtcHideLocalIpsWithMdns",
                        "about:blank",
                    ],
                    stdout=subprocess.DEVNULL,
                    stderr=stderr,
                    start_new_session=True,
                )

            port = await wait_for_devtools_port(
                profile,
                chrome,
                min(args.timeout, 15),
            )
            page = await create_page(
                port,
                f"{args.base_url}/health/live",
                min(args.timeout, 15),
            )
            config = {
                "baseUrl": args.base_url,
                "signalTimeoutMs": int(min(args.timeout, 15) * 1000),
                "streamId": stream_id,
            }
            await page.evaluate(
                BOOTSTRAP_JS.replace(
                    "__CONFIG_JSON__",
                    json.dumps(config, separators=(",", ":")),
                )
            )

            async def snapshot() -> Any:
                nonlocal last_snapshot
                last_snapshot = await page.evaluate(
                    "window.__aeroWhepSmoke?.snapshot?.()"
                    " || {error:'WHEP smoke state unavailable'}"
                )
                return last_snapshot

            before = await wait_for(
                "connected WHEP H.264 playback",
                time.monotonic() + args.timeout,
                snapshot,
                playback_ready,
            )
            await asyncio.sleep(args.progress_seconds)
            after = await snapshot()
            if not playback_ready(after):
                raise SmokeFailure("WHEP playback stopped after initial acceptance")
            before_inbound = before["inbound"]
            after_inbound = after["inbound"]
            if (
                after_inbound["packetsReceived"]
                <= before_inbound["packetsReceived"]
                or after_inbound["bytesReceived"] <= before_inbound["bytesReceived"]
            ):
                raise SmokeFailure(
                    "inbound H.264 RTP packet/byte counters did not keep advancing"
                )
            shutdown = await page.evaluate(
                "window.__aeroWhepSmoke?.shutdown?.()"
                " || Promise.resolve({deleteStatus:null})"
            )
            delete_status = (
                shutdown.get("deleteStatus") if isinstance(shutdown, dict) else None
            )
            if delete_status != 204:
                raise SmokeFailure(
                    f"WHEP resource DELETE returned unexpected status {delete_status}"
                )
            print(
                json.dumps(
                    safe_result(stream_id, before, after, delete_status),
                    ensure_ascii=False,
                    indent=2,
                )
            )
    except Exception:
        diagnostic = {
            "ok": False,
            "stream_id": stream_id,
            "last_browser_snapshot": last_snapshot,
        }
        if chrome_log_path is not None:
            with contextlib.suppress(OSError):
                lines = chrome_log_path.read_text(
                    encoding="utf-8",
                    errors="replace",
                ).splitlines()
                diagnostic["chrome_stderr_tail"] = "\n".join(lines[-60:])
        print(
            redact(
                json.dumps(diagnostic, ensure_ascii=False, indent=2),
                secrets_to_hide,
            ),
            file=sys.stderr,
        )
        raise
    finally:
        if page is not None:
            with contextlib.suppress(Exception):
                await page.evaluate(
                    "window.__aeroWhepSmoke?.shutdown?.()"
                    " || Promise.resolve({deleteStatus:null})"
                )
            with contextlib.suppress(Exception):
                await page.close()
        terminate_chrome(chrome)
        publisher_stderr = stop_process(publisher)
        if publisher is not None and publisher.returncode not in {0, -signal.SIGTERM}:
            tail = redact(publisher_stderr[-1200:], secrets_to_hide)
            if tail.strip():
                print(f"FFmpeg publisher diagnostics:\n{tail}", file=sys.stderr)


def executable(value: str) -> str:
    resolved = shutil.which(value)
    if resolved is None:
        raise argparse.ArgumentTypeError(f"executable not found: {value}")
    return resolved


def validate_ffmpeg(ffmpeg: str) -> None:
    muxers = subprocess.run(
        [ffmpeg, "-hide_banner", "-muxers"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=10,
    ).stdout
    encoders = subprocess.run(
        [ffmpeg, "-hide_banner", "-encoders"],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=10,
    ).stdout
    if not any(line.rstrip().endswith("whip") or " whip " in line for line in muxers.splitlines()):
        raise SmokeFailure(f"{ffmpeg} has no WHIP muxer")
    if "libx264" not in encoders:
        raise SmokeFailure(f"{ffmpeg} has no libx264 encoder")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Publish H.264 over WHIP and prove sustained WHEP playback in "
            "headless Chrome."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--base-url",
        default=os.environ.get(
            "AERO_WHEP_SMOKE_BASE_URL",
            os.environ.get("AERO_HOST", "http://127.0.0.1:3030"),
        ),
        help="running Aero HTTP origin",
    )
    parser.add_argument(
        "--chrome",
        type=executable,
        default=shutil.which("google-chrome") or shutil.which("chromium"),
    )
    parser.add_argument(
        "--ffmpeg",
        type=executable,
        default=shutil.which("ffmpeg"),
        help="FFmpeg build with the WHIP muxer and libx264",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=30,
        help="timeout for server, signaling and media acceptance",
    )
    parser.add_argument(
        "--progress-seconds",
        type=float,
        default=2.5,
        help="second getStats sample delay used to prove sustained RTP",
    )
    args = parser.parse_args(argv)
    if websockets is None:
        parser.error("Python package 'websockets' is required")
    if not args.chrome:
        parser.error("Google Chrome or Chromium is required")
    if not args.ffmpeg:
        parser.error("FFmpeg is required")
    parsed = urllib.parse.urlsplit(args.base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        parser.error("--base-url must be an absolute http(s) origin")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        parser.error("--base-url must not include a path, query or fragment")
    if args.timeout < 5 or args.timeout > 180:
        parser.error("--timeout must be between 5 and 180 seconds")
    if args.progress_seconds < 1 or args.progress_seconds > 30:
        parser.error("--progress-seconds must be between 1 and 30")
    args.base_url = args.base_url.rstrip("/")
    validate_ffmpeg(args.ffmpeg)
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    try:
        asyncio.run(run_smoke(args))
    except (SmokeFailure, subprocess.TimeoutExpired) as error:
        print(f"WHEP browser smoke failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
