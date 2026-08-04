#!/usr/bin/env python3
"""Real OBS Studio RTMP ingest smoke for Aero IM.

This is deliberately not an FFmpeg-publisher equivalence test. FFmpeg is used
only to create a deterministic local H.264/AAC fixture. OBS Studio loads that
file as a media source, encodes the active scene with obs-x264 + FFmpeg AAC,
and publishes the resulting FLV stream to Aero's production RTMP listener.

The smoke proves all of the following:

* an isolated OBS profile and scene load under Xvfb;
* OBS' own WebSocket API reports an active stream with advancing byte counts;
* Aero produces a progressing HLS playlist from the OBS RTMP session;
* ffprobe sees sustained H.264 video and AAC audio in that HLS output; and
* OBS, Xvfb, credentials and temporary configuration are cleaned up.

The Python ``websockets`` package, OBS Studio 30.x, Xvfb, FFmpeg and ffprobe
must be installed. Generated credentials and the stream key are kept in memory
and redacted from diagnostics.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import contextlib
import hashlib
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
import uuid
from typing import Any, Callable

try:
    import websockets
except ModuleNotFoundError:
    websockets = None  # type: ignore[assignment]


PROFILE_NAME = "AeroObsSmoke"
COLLECTION_NAME = "AeroObsSmoke"
SCENE_NAME = "Aero OBS Smoke"
INPUT_NAME = "Aero H264 AAC Fixture"
MAX_HTTP_BODY = 1024 * 1024


class SmokeFailure(RuntimeError):
    """An expected staging or acceptance failure."""


class ObsRequestFailure(SmokeFailure):
    """An OBS WebSocket request failed with a protocol status code."""

    def __init__(
        self,
        request_type: str,
        code: Any,
        comment: Any,
    ) -> None:
        self.request_type = request_type
        self.code = code
        self.comment = comment
        super().__init__(
            f"OBS {request_type} failed ({code}): {comment or ''}"
        )


def executable(value: str) -> str:
    resolved = shutil.which(value)
    if resolved is None:
        raise argparse.ArgumentTypeError(f"executable not found: {value}")
    return resolved


def redact(text: str, secrets_to_hide: tuple[str, ...]) -> str:
    redacted = text
    for secret in sorted(
        (value for value in secrets_to_hide if value),
        key=len,
        reverse=True,
    ):
        redacted = redacted.replace(secret, "<redacted>")
        redacted = redacted.replace(
            urllib.parse.quote(secret, safe=""),
            "<redacted>",
        )
    return redacted


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
            raw = response.read(MAX_HTTP_BODY + 1)
    except urllib.error.HTTPError as error:
        status = error.code
        raw = error.read(MAX_HTTP_BODY + 1)
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        reason = getattr(error, "reason", error)
        raise SmokeFailure(
            f"{method} {path} failed: {type(reason).__name__}: {reason}"
        ) from error
    if status not in accepted:
        detail = raw[:300].decode(errors="replace")
        raise SmokeFailure(
            f"{method} {path} returned HTTP {status}: {detail}"
        )
    if len(raw) > MAX_HTTP_BODY:
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
            raise SmokeFailure(
                f"{operation} response omitted {'.'.join(path)}"
            )
        value = value[key]
    if not isinstance(value, str) or not value.strip():
        raise SmokeFailure(
            f"{operation} response field {'.'.join(path)} was invalid"
        )
    return value


def wait_for_server(base_url: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error = "not attempted"
    while time.monotonic() < deadline:
        try:
            request = urllib.request.Request(
                f"{base_url.rstrip('/')}/health/ready",
                headers={"accept": "application/json"},
            )
            with urllib.request.urlopen(request, timeout=2) as response:
                if response.status == 200:
                    return
                last_error = f"HTTP {response.status}"
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            last_error = str(getattr(error, "reason", error))
        time.sleep(0.25)
    raise SmokeFailure(f"Aero readiness timed out: {last_error}")


def provision_stream(
    base_url: str,
    timeout: float,
) -> tuple[str, str, str, str, str]:
    unique = f"{time.time_ns():x}-{secrets.token_hex(5)}"
    password = secrets.token_urlsafe(32)
    registered = api_json(
        base_url,
        "POST",
        "/api/auth/register",
        body={
            "display_name": "OBS Ingest Smoke",
            "email": f"obs-smoke+{unique}@aero.dev",
            "password": password,
        },
        timeout=timeout,
    )
    token = required_text(registered, "register", "access_token")
    stream = api_json(
        base_url,
        "POST",
        "/api/streams",
        body={"protocol": "rtmp", "title": f"obs-smoke-{unique}"},
        token=token,
        timeout=timeout,
    )
    stream_id = required_text(stream, "create stream", "id")
    stream_key = required_text(stream, "create stream", "stream_key")
    ingest_url = required_text(stream, "create stream", "ingest_url")
    parsed = urllib.parse.urlsplit(ingest_url)
    if (
        parsed.scheme != "rtmp"
        or not parsed.hostname
        or not parsed.port
        or "/" not in parsed.path.strip("/")
    ):
        raise SmokeFailure("create stream returned an invalid RTMP ingest URL")
    path_prefix, encoded_key = parsed.path.rsplit("/", 1)
    if urllib.parse.unquote(encoded_key) != stream_key:
        raise SmokeFailure("RTMP ingest URL does not end in the stream key")
    server_url = urllib.parse.urlunsplit(
        (parsed.scheme, parsed.netloc, path_prefix, "", "")
    )
    return token, stream_id, stream_key, ingest_url, server_url


def command_output(command: list[str], timeout: float = 15) -> str:
    result = subprocess.run(
        command,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    if result.returncode != 0:
        raise SmokeFailure(
            f"{pathlib.Path(command[0]).name} exited {result.returncode}: "
            f"{result.stdout[-800:]}"
        )
    return result.stdout.strip()


def validate_tools(args: argparse.Namespace) -> str:
    version = command_output([args.obs, "--version"])
    if "OBS Studio" not in version:
        raise SmokeFailure(f"unexpected OBS version output: {version}")
    try:
        major_text = version.split("-", 1)[1].strip().split(".", 1)[0]
        major = int(major_text)
    except (IndexError, ValueError) as error:
        raise SmokeFailure(f"cannot parse OBS version: {version}") from error
    if major < 30:
        raise SmokeFailure(f"OBS Studio 30.x or newer is required: {version}")
    encoders = command_output(
        [args.ffmpeg, "-hide_banner", "-encoders"],
        timeout=20,
    )
    if "libx264" not in encoders or " aac " not in encoders:
        raise SmokeFailure("FFmpeg fixture generator needs libx264 and AAC")
    return version


def generate_fixture(
    ffmpeg: str,
    destination: pathlib.Path,
    duration: float,
) -> None:
    fixture_duration = max(8.0, min(duration, 12.0))
    command = [
        ffmpeg,
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=880:sample_rate=48000",
        "-t",
        f"{fixture_duration:.3f}",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-profile:v",
        "baseline",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "60",
        "-keyint_min",
        "60",
        "-sc_threshold",
        "0",
        "-bf",
        "0",
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-movflags",
        "+faststart",
        "-shortest",
        "-y",
        str(destination),
    ]
    result = subprocess.run(
        command,
        check=False,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        timeout=45,
    )
    if result.returncode != 0 or not destination.is_file():
        raise SmokeFailure(
            f"fixture FFmpeg exited {result.returncode}: "
            f"{result.stderr[-1000:]}"
        )


def write_obs_config(
    root: pathlib.Path,
    websocket_port: int,
    websocket_password: str,
    server_url: str,
    stream_key: str,
) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
    home = root / "home"
    config_home = root / "xdg-config"
    runtime = root / "xdg-runtime"
    home.mkdir(parents=True)
    runtime.mkdir(parents=True)
    runtime.chmod(0o700)
    obs_root = config_home / "obs-studio"
    profile_dir = obs_root / "basic" / "profiles" / PROFILE_NAME
    scenes_dir = obs_root / "basic" / "scenes"
    profile_dir.mkdir(parents=True)
    scenes_dir.mkdir(parents=True)

    global_ini = f"""\
[General]
FirstRun=false
MaxLogs=10
ProcessPriority=Normal
EnableAutoUpdates=false
ConfirmOnExit=false

[Video]
Renderer=OpenGL

[BasicWindow]
PreviewEnabled=false
SysTrayEnabled=false
SysTrayWhenStarted=false
SaveProjectors=false
ShowStatusBar=true

[Basic]
Profile={PROFILE_NAME}
ProfileDir={PROFILE_NAME}
SceneCollection={COLLECTION_NAME}
SceneCollectionFile={COLLECTION_NAME}
ConfigOnNewProfile=false

[OBSWebSocket]
FirstLoad=false
ServerEnabled=true
ServerPort={websocket_port}
AlertsEnabled=false
AuthRequired=true
ServerPassword={websocket_password}
"""
    (obs_root / "global.ini").write_text(global_ini, encoding="utf-8")

    basic_ini = f"""\
[General]
Name={PROFILE_NAME}

[Output]
Mode=Simple
RetryDelay=1
MaxRetries=3
DelayEnable=false
Reconnect=true

[SimpleOutput]
VBitrate=900
StreamEncoder=x264
ABitrate=96
UseAdvanced=true
Preset=veryfast
EnforceBitrate=true
x264Settings=keyint=60 scenecut=0

[Video]
BaseCX=640
BaseCY=360
OutputCX=640
OutputCY=360
FPSType=0
FPSCommon=30
ScaleType=bicubic
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo
"""
    (profile_dir / "basic.ini").write_text(basic_ini, encoding="utf-8")
    service = {
        "settings": {
            "bwtest": False,
            "key": stream_key,
            "server": server_url,
            "service": "Custom",
            "use_auth": False,
        },
        "type": "rtmp_custom",
    }
    (profile_dir / "service.json").write_text(
        json.dumps(service, separators=(",", ":")),
        encoding="utf-8",
    )

    scene_uuid = str(uuid.uuid4())
    scene_collection = {
        "current_scene": SCENE_NAME,
        "current_program_scene": SCENE_NAME,
        "scene_order": [{"name": SCENE_NAME}],
        "name": COLLECTION_NAME,
        "sources": [
            {
                "prev_ver": 503316482,
                "name": SCENE_NAME,
                "uuid": scene_uuid,
                "id": "scene",
                "versioned_id": "scene",
                "settings": {
                    "id_counter": 0,
                    "custom_size": False,
                    "items": [],
                },
                "mixers": 0,
                "sync": 0,
                "flags": 0,
                "volume": 1.0,
                "balance": 0.5,
                "enabled": True,
                "muted": False,
                "push-to-mute": False,
                "push-to-mute-delay": 0,
                "push-to-talk": False,
                "push-to-talk-delay": 0,
                "hotkeys": {"OBSBasic.SelectScene": []},
                "deinterlace_mode": 0,
                "deinterlace_field_order": 0,
                "monitoring_type": 0,
                "private_settings": {},
            }
        ],
        "groups": [],
        "quick_transitions": [
            {
                "name": "Cut",
                "duration": 300,
                "hotkeys": [],
                "id": 1,
                "fade_to_black": False,
            },
            {
                "name": "Fade",
                "duration": 300,
                "hotkeys": [],
                "id": 2,
                "fade_to_black": False,
            },
        ],
        "transitions": [],
        "saved_projectors": [],
        "current_transition": "Fade",
        "transition_duration": 300,
        "preview_locked": False,
        "scaling_enabled": False,
        "scaling_level": 0,
        "scaling_off_x": 0.0,
        "scaling_off_y": 0.0,
        "virtual-camera": {"type2": 3},
        "modules": {},
    }
    (scenes_dir / f"{COLLECTION_NAME}.json").write_text(
        json.dumps(scene_collection, separators=(",", ":")),
        encoding="utf-8",
    )
    return home, config_home, runtime


def allocate_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def start_xvfb(
    xvfb: str,
    root: pathlib.Path,
) -> tuple[subprocess.Popen[str], str, Any]:
    log_handle = (root / "xvfb.log").open("w", encoding="utf-8")
    for display_number in range(90, 190):
        display = f":{display_number}"
        socket_path = pathlib.Path(f"/tmp/.X11-unix/X{display_number}")
        if socket_path.exists():
            continue
        process = subprocess.Popen(
            [
                xvfb,
                display,
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ],
            text=True,
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        for _attempt in range(30):
            if socket_path.exists():
                return process, display, log_handle
            if process.poll() is not None:
                break
            time.sleep(0.1)
        stop_process(process, signal.SIGTERM)
    log_handle.close()
    raise SmokeFailure("could not allocate an Xvfb display")


def start_obs(
    args: argparse.Namespace,
    root: pathlib.Path,
    display: str,
    home: pathlib.Path,
    config_home: pathlib.Path,
    runtime: pathlib.Path,
) -> tuple[subprocess.Popen[str], Any]:
    log_handle = (root / "obs-console.log").open("w", encoding="utf-8")
    env = os.environ.copy()
    env.update(
        {
            "DISPLAY": display,
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(config_home),
            "XDG_RUNTIME_DIR": str(runtime),
            "QT_QPA_PLATFORM": "xcb",
            "LIBGL_ALWAYS_SOFTWARE": "1",
            "LC_ALL": "C.UTF-8",
        }
    )
    process = subprocess.Popen(
        [
            args.obs,
            "--multi",
            "--only-bundled-plugins",
            "--disable-shutdown-check",
            "--disable-missing-files-check",
            "--profile",
            PROFILE_NAME,
            "--collection",
            COLLECTION_NAME,
            "--scene",
            SCENE_NAME,
            "--startstreaming",
            "--verbose",
        ],
        env=env,
        text=True,
        stdout=log_handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    return process, log_handle


def stop_process(
    process: subprocess.Popen[Any] | None,
    initial_signal: signal.Signals,
    timeout: float = 10,
) -> int | None:
    if process is None:
        return None
    if process.poll() is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, initial_signal)
        try:
            return process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                return process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                return process.wait(timeout=5)
    return process.returncode


class ObsWebSocket:
    """Minimal OBS WebSocket 5.x JSON client."""

    def __init__(self, connection: Any) -> None:
        self.connection = connection

    @classmethod
    async def connect(
        cls,
        url: str,
        password: str,
        process: subprocess.Popen[Any],
        timeout: float,
    ) -> "ObsWebSocket":
        deadline = time.monotonic() + timeout
        last_error = "not attempted"
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise SmokeFailure(
                    f"OBS exited before WebSocket became ready: "
                    f"{process.returncode}"
                )
            try:
                connection = await websockets.connect(
                    url,
                    open_timeout=2,
                    close_timeout=2,
                    max_size=MAX_HTTP_BODY,
                )
                raw_hello = await asyncio.wait_for(connection.recv(), timeout=3)
                hello = json.loads(raw_hello)
                if hello.get("op") != 0:
                    raise SmokeFailure("OBS WebSocket omitted Hello")
                authentication = hello.get("d", {}).get("authentication")
                identify: dict[str, Any] = {"rpcVersion": 1}
                if authentication:
                    salt = str(authentication["salt"])
                    challenge = str(authentication["challenge"])
                    secret = base64.b64encode(
                        hashlib.sha256(f"{password}{salt}".encode()).digest()
                    ).decode()
                    identify["authentication"] = base64.b64encode(
                        hashlib.sha256(
                            f"{secret}{challenge}".encode()
                        ).digest()
                    ).decode()
                await connection.send(json.dumps({"op": 1, "d": identify}))
                raw_identified = await asyncio.wait_for(
                    connection.recv(),
                    timeout=3,
                )
                identified = json.loads(raw_identified)
                if identified.get("op") != 2:
                    await connection.close()
                    raise SmokeFailure(
                        f"OBS WebSocket identification failed: {identified}"
                    )
                return cls(connection)
            except SmokeFailure:
                raise
            except (
                OSError,
                TimeoutError,
                asyncio.TimeoutError,
                json.JSONDecodeError,
            ) as error:
                last_error = f"{type(error).__name__}: {error}"
                await asyncio.sleep(0.2)
        raise SmokeFailure(f"OBS WebSocket readiness timed out: {last_error}")

    async def request(
        self,
        request_type: str,
        request_data: dict[str, Any] | None = None,
        timeout: float = 15,
    ) -> dict[str, Any]:
        request_id = secrets.token_hex(10)
        data: dict[str, Any] = {
            "requestType": request_type,
            "requestId": request_id,
        }
        if request_data is not None:
            data["requestData"] = request_data
        await self.connection.send(json.dumps({"op": 6, "d": data}))
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            raw = await asyncio.wait_for(
                self.connection.recv(),
                timeout=max(0.1, deadline - time.monotonic()),
            )
            message = json.loads(raw)
            if message.get("op") != 7:
                continue
            response = message.get("d", {})
            if response.get("requestId") != request_id:
                continue
            status = response.get("requestStatus", {})
            if not status.get("result"):
                raise ObsRequestFailure(
                    request_type,
                    status.get("code"),
                    status.get("comment"),
                )
            body = response.get("responseData", {})
            return body if isinstance(body, dict) else {}
        raise SmokeFailure(f"OBS {request_type} timed out")

    async def close(self) -> None:
        await self.connection.close()


async def wait_obs_condition(
    description: str,
    timeout: float,
    operation: Callable[[], Any],
    predicate: Callable[[dict[str, Any]], bool],
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        last = await operation()
        if predicate(last):
            return last
        await asyncio.sleep(0.25)
    raise SmokeFailure(f"{description} timed out; last={last}")


async def wait_obs_ready(
    client: ObsWebSocket,
    process: subprocess.Popen[Any],
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last_error = "OBS WebSocket connected but frontend was not ready"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise SmokeFailure(
                f"OBS exited during frontend startup: {process.returncode}"
            )
        try:
            version = await client.request("GetVersion", timeout=3)
            # The WebSocket server can accept connections just before OBS'
            # frontend finishes constructing its output handlers.
            await asyncio.sleep(1)
            return version
        except ObsRequestFailure as error:
            if error.code != 207:
                raise
            last_error = str(error)
            await asyncio.sleep(0.2)
    raise SmokeFailure(f"OBS frontend readiness timed out: {last_error}")


def fetch_playlist(url: str, timeout: float = 5) -> str | None:
    request = urllib.request.Request(url, headers={"accept": "*/*"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read(MAX_HTTP_BODY + 1)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise SmokeFailure(f"HLS GET returned HTTP {error.code}") from error
    except (urllib.error.URLError, TimeoutError, OSError):
        return None
    if len(raw) > MAX_HTTP_BODY:
        raise SmokeFailure("HLS playlist exceeded 1 MiB")
    return raw.decode(errors="replace")


def playlist_segments(playlist: str | None) -> list[str]:
    if not playlist:
        return []
    return [
        line.strip()
        for line in playlist.splitlines()
        if line.strip() and not line.startswith("#")
    ]


def probe_hls(ffprobe: str, hls_url: str, minimum_duration: float) -> dict[str, Any]:
    result = subprocess.run(
        [
            ffprobe,
            "-v",
            "error",
            "-rw_timeout",
            "15000000",
            "-count_packets",
            "-show_entries",
            (
                "format=duration:"
                "stream=index,codec_type,codec_name,width,height,"
                "sample_rate,channels,avg_frame_rate,nb_read_packets"
            ),
            "-of",
            "json",
            hls_url,
        ],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=30,
    )
    if result.returncode != 0:
        raise SmokeFailure(
            f"ffprobe exited {result.returncode}: {result.stderr[-1000:]}"
        )
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise SmokeFailure(
            "ffprobe returned invalid JSON; stderr tail: "
            f"{result.stderr[-1000:]}"
        ) from error
    streams = document.get("streams", [])
    video = next(
        (
            item
            for item in streams
            if item.get("codec_type") == "video"
            and item.get("codec_name") == "h264"
        ),
        None,
    )
    audio = next(
        (
            item
            for item in streams
            if item.get("codec_type") == "audio"
            and item.get("codec_name") == "aac"
        ),
        None,
    )
    if video is None:
        raise SmokeFailure(f"OBS HLS lacks H.264 video: {streams}")
    if audio is None:
        raise SmokeFailure(f"OBS HLS lacks AAC audio: {streams}")
    try:
        duration = float(document.get("format", {}).get("duration"))
    except (TypeError, ValueError) as error:
        raise SmokeFailure("OBS HLS lacks a valid duration") from error
    if duration < minimum_duration:
        raise SmokeFailure(
            f"OBS HLS duration {duration:.3f}s is below "
            f"{minimum_duration:.3f}s"
        )
    for label, stream in (("video", video), ("audio", audio)):
        try:
            packets = int(stream.get("nb_read_packets"))
        except (TypeError, ValueError) as error:
            raise SmokeFailure(
                f"ffprobe did not count {label} packets: {stream}"
            ) from error
        if packets <= 0:
            raise SmokeFailure(f"ffprobe counted no {label} packets")
    return {
        "duration_seconds": round(duration, 3),
        "video": video,
        "audio": audio,
    }


def latest_obs_log(config_home: pathlib.Path) -> str:
    logs_dir = config_home / "obs-studio" / "logs"
    logs = sorted(
        logs_dir.glob("*.txt"),
        key=lambda path: path.stat().st_mtime,
    )
    if not logs:
        return ""
    return logs[-1].read_text(encoding="utf-8", errors="replace")


def obs_log_evidence(log_text: str) -> list[str]:
    needles = (
        "obs-studio",
        "obs-websocket",
        "obs_x264",
        "ffmpeg_aac",
        "rtmp",
        "streaming start",
        "streaming stop",
        "total frames output",
        "dropped frames",
    )
    selected = [
        line.strip()
        for line in log_text.splitlines()
        if any(needle in line.lower() for needle in needles)
        and "kbit/s" not in line
    ]
    return selected[-24:]


def obs_failure_evidence(log_text: str) -> list[str]:
    """Keep startup/output failures that repeated status polling can bury."""
    needles = (
        "error",
        "warning",
        "fail",
        "invalid",
        "encoder",
        "output",
        "service",
        "rtmp",
        "stream",
    )
    selected = [
        line.strip()
        for line in log_text.splitlines()
        if any(needle in line.lower() for needle in needles)
    ]
    return selected[-160:]


def integer_field(document: dict[str, Any], name: str) -> int:
    value = document.get(name, 0)
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


async def run_obs_session(
    args: argparse.Namespace,
    root: pathlib.Path,
    obs_version: str,
    token: str,
    stream_id: str,
    stream_key: str,
    ingest_url: str,
    server_url: str,
) -> dict[str, Any]:
    websocket_port = allocate_tcp_port()
    websocket_password = secrets.token_urlsafe(32)
    fixture = root / "obs-fixture.mp4"
    await asyncio.to_thread(
        generate_fixture,
        args.ffmpeg,
        fixture,
        args.duration,
    )
    home, config_home, runtime = write_obs_config(
        root,
        websocket_port,
        websocket_password,
        server_url,
        stream_key,
    )
    xvfb_process: subprocess.Popen[str] | None = None
    obs_process: subprocess.Popen[str] | None = None
    xvfb_log_handle: Any = None
    obs_log_handle: Any = None
    client: ObsWebSocket | None = None
    stream_started = False
    obs_exit_code: int | None = None
    xvfb_exit_code: int | None = None
    last_status: dict[str, Any] = {}
    secrets_to_hide = (
        stream_key,
        ingest_url,
        token,
        websocket_password,
    )
    try:
        xvfb_process, display, xvfb_log_handle = start_xvfb(args.xvfb, root)
        obs_process, obs_log_handle = start_obs(
            args,
            root,
            display,
            home,
            config_home,
            runtime,
        )
        client = await ObsWebSocket.connect(
            f"ws://127.0.0.1:{websocket_port}",
            websocket_password,
            obs_process,
            args.timeout,
        )
        version_data = await wait_obs_ready(
            client,
            obs_process,
            args.timeout,
        )
        await client.request(
            "SetCurrentProgramScene",
            {"sceneName": SCENE_NAME},
        )
        created = await client.request(
            "CreateInput",
            {
                "sceneName": SCENE_NAME,
                "inputName": INPUT_NAME,
                "inputKind": "ffmpeg_source",
                "inputSettings": {
                    "is_local_file": True,
                    "local_file": str(fixture),
                    "looping": True,
                    "restart_on_activate": True,
                    "close_when_inactive": False,
                    "clear_on_media_end": False,
                    "hw_decode": False,
                },
                "sceneItemEnabled": True,
            },
        )
        media_status = await wait_obs_condition(
            "OBS media source playback",
            min(args.timeout, 15),
            lambda: client.request(
                "GetMediaInputStatus",
                {"inputName": INPUT_NAME},
            ),
            lambda value: value.get("mediaState")
            in {
                "OBS_MEDIA_STATE_PLAYING",
                "OBS_MEDIA_STATE_OPENING",
                "OBS_MEDIA_STATE_BUFFERING",
            },
        )
        service_settings = await client.request("GetStreamServiceSettings")
        returned_settings = service_settings.get(
            "streamServiceSettings",
            {},
        )
        if returned_settings.get("server") != server_url:
            raise SmokeFailure("OBS did not retain the custom RTMP service")

        initial_status = await wait_obs_condition(
            "OBS --startstreaming activation",
            args.timeout,
            lambda: client.request("GetStreamStatus"),
            lambda value: value.get("outputActive") is True,
        )
        stream_started = True
        hls_url = (
            f"{args.base_url.rstrip('/')}/hls/{stream_id}/index.m3u8"
        )
        observed_segments: set[str] = set()
        first_playlist: str | None = None
        last_playlist: str | None = None
        started_at = time.monotonic()
        deadline = started_at + args.duration
        while time.monotonic() < deadline:
            if obs_process.poll() is not None:
                raise SmokeFailure(
                    f"OBS exited during streaming: {obs_process.returncode}"
                )
            last_status = await client.request("GetStreamStatus")
            if last_status.get("outputActive") is not True:
                raise SmokeFailure(
                    f"OBS stream became inactive: {last_status}"
                )
            playlist = await asyncio.to_thread(fetch_playlist, hls_url)
            if playlist and "#EXTM3U" in playlist:
                first_playlist = first_playlist or playlist
                last_playlist = playlist
                observed_segments.update(playlist_segments(playlist))
            await asyncio.sleep(0.5)
        final_active_status = await client.request("GetStreamStatus")
        if integer_field(final_active_status, "outputBytes") <= integer_field(
            initial_status,
            "outputBytes",
        ):
            raise SmokeFailure("OBS output byte counter did not advance")
        if not last_playlist:
            raise SmokeFailure("Aero produced no HLS playlist from OBS")

        await client.request("StopStream", timeout=args.timeout)
        stream_started = False
        stopped_status = await wait_obs_condition(
            "OBS stream stop",
            min(args.timeout, 15),
            lambda: client.request("GetStreamStatus"),
            lambda value: value.get("outputActive") is False,
        )
        stabilization_deadline = time.monotonic() + 10
        while time.monotonic() < stabilization_deadline:
            playlist = await asyncio.to_thread(fetch_playlist, hls_url)
            if playlist and "#EXTM3U" in playlist:
                last_playlist = playlist
                observed_segments.update(playlist_segments(playlist))
                if "#EXT-X-ENDLIST" in playlist:
                    break
            await asyncio.sleep(0.25)
        if len(observed_segments) < 2:
            raise SmokeFailure(
                "OBS HLS did not progress across at least two segments"
            )
        if first_playlist == last_playlist:
            raise SmokeFailure("OBS HLS playlist did not change while live")

        api_json(
            args.base_url,
            "POST",
            f"/api/streams/{stream_id}/end",
            token=token,
            accepted=(200, 409),
            timeout=args.timeout,
        )
        minimum_duration = max(4.0, min(args.duration * 0.45, 7.0))
        probe = await asyncio.to_thread(
            probe_hls,
            args.ffprobe,
            hls_url,
            minimum_duration,
        )
        await client.close()
        client = None
        obs_exit_code = await asyncio.to_thread(
            stop_process,
            obs_process,
            signal.SIGINT,
        )
        xvfb_exit_code = await asyncio.to_thread(
            stop_process,
            xvfb_process,
            signal.SIGTERM,
        )
        if obs_log_handle is not None:
            obs_log_handle.close()
            obs_log_handle = None
        if xvfb_log_handle is not None:
            xvfb_log_handle.close()
            xvfb_log_handle = None
        obs_log = latest_obs_log(config_home)
        evidence = [
            redact(line, secrets_to_hide)
            for line in obs_log_evidence(obs_log)
        ]
        output_before = integer_field(initial_status, "outputBytes")
        output_after = integer_field(final_active_status, "outputBytes")
        return {
            "ok": True,
            "stream_id": stream_id,
            "obs": {
                "binary_version": obs_version,
                "reported_version": version_data.get("obsVersion"),
                "websocket_version": version_data.get(
                    "obsWebSocketVersion"
                ),
                "rpc_version": version_data.get("rpcVersion"),
                "pid": obs_process.pid,
                "exit_code": obs_exit_code,
                "media_state": media_status.get("mediaState"),
                "scene_item_id": created.get("sceneItemId"),
                "output_bytes": output_after,
                "output_bytes_delta": output_after - output_before,
                "output_duration_ms": integer_field(
                    final_active_status,
                    "outputDuration",
                ),
                "output_skipped_frames": integer_field(
                    final_active_status,
                    "outputSkippedFrames",
                ),
                "output_congestion": final_active_status.get(
                    "outputCongestion",
                ),
                "stopped": stopped_status.get("outputActive") is False,
                "log_evidence": evidence,
            },
            "xvfb": {
                "pid": xvfb_process.pid,
                "display": display,
                "exit_code": xvfb_exit_code,
            },
            "hls": {
                "url": hls_url,
                "segments_observed": len(observed_segments),
                "playlist_progressed": first_playlist != last_playlist,
                **probe,
            },
        }
    except BaseException:
        diagnostic_parts: list[str] = []
        with contextlib.suppress(OSError):
            obs_log = latest_obs_log(config_home)
            diagnostic_parts.append(
                "\n".join(obs_failure_evidence(obs_log))
            )
            diagnostic_parts.append(obs_log[-4000:])
        with contextlib.suppress(OSError):
            console_log = (root / "obs-console.log").read_text(
                encoding="utf-8",
                errors="replace",
            )
            diagnostic_parts.append(
                "\n".join(obs_failure_evidence(console_log))
            )
            diagnostic_parts.append(console_log[-4000:])
        diagnostic = redact(
            "\n".join(diagnostic_parts),
            secrets_to_hide,
        )
        if diagnostic.strip():
            print(
                "OBS diagnostics:\n" + diagnostic[-24000:],
                file=sys.stderr,
            )
        raise
    finally:
        if client is not None:
            if stream_started:
                with contextlib.suppress(Exception):
                    await client.request("StopStream", timeout=5)
            with contextlib.suppress(Exception):
                await client.close()
        if obs_exit_code is None:
            obs_exit_code = await asyncio.to_thread(
                stop_process,
                obs_process,
                signal.SIGINT,
            )
        if xvfb_exit_code is None:
            xvfb_exit_code = await asyncio.to_thread(
                stop_process,
                xvfb_process,
                signal.SIGTERM,
            )
        if obs_log_handle is not None:
            obs_log_handle.close()
        if xvfb_log_handle is not None:
            xvfb_log_handle.close()


async def run_smoke(args: argparse.Namespace) -> dict[str, Any]:
    obs_version = await asyncio.to_thread(validate_tools, args)
    await asyncio.to_thread(wait_for_server, args.base_url, args.timeout)
    token, stream_id, stream_key, ingest_url, server_url = (
        await asyncio.to_thread(
            provision_stream,
            args.base_url,
            args.timeout,
        )
    )
    temp_path = ""
    result: dict[str, Any]
    with tempfile.TemporaryDirectory(prefix="aero-obs-smoke-") as temp_dir:
        temp_path = temp_dir
        result = await run_obs_session(
            args,
            pathlib.Path(temp_dir),
            obs_version,
            token,
            stream_id,
            stream_key,
            ingest_url,
            server_url,
        )
    result["cleanup"] = {
        "temp_dir": temp_path,
        "temp_removed": not pathlib.Path(temp_path).exists(),
    }
    return result


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Use real OBS Studio to publish RTMP and prove Aero HLS contains "
            "sustained H.264 video plus AAC audio."
        ),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--base-url",
        default=os.environ.get(
            "AERO_OBS_SMOKE_BASE_URL",
            os.environ.get("AERO_HOST", "http://127.0.0.1:3030"),
        ),
        help="running Aero HTTP origin",
    )
    parser.add_argument(
        "--obs",
        type=executable,
        default=shutil.which("obs"),
        help="OBS Studio 30.x binary",
    )
    parser.add_argument(
        "--xvfb",
        type=executable,
        default=shutil.which("Xvfb"),
        help="X virtual framebuffer binary",
    )
    parser.add_argument(
        "--ffmpeg",
        type=executable,
        default=shutil.which("ffmpeg"),
        help="fixture generator with libx264 and AAC",
    )
    parser.add_argument(
        "--ffprobe",
        type=executable,
        default=shutil.which("ffprobe"),
        help="HLS media verifier",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=18,
        help="seconds OBS must remain actively streaming",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=45,
        help="readiness and signaling timeout",
    )
    args = parser.parse_args(argv)
    if websockets is None:
        parser.error("Python package 'websockets' is required")
    for name in ("obs", "xvfb", "ffmpeg", "ffprobe"):
        if not getattr(args, name):
            parser.error(f"--{name} executable is required")
    parsed = urllib.parse.urlsplit(args.base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        parser.error("--base-url must be an absolute http(s) origin")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        parser.error("--base-url must not include a path, query or fragment")
    if args.duration < 10 or args.duration > 120:
        parser.error("--duration must be between 10 and 120 seconds")
    if args.timeout < 10 or args.timeout > 180:
        parser.error("--timeout must be between 10 and 180 seconds")
    args.base_url = args.base_url.rstrip("/")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    try:
        result = asyncio.run(run_smoke(args))
    except (SmokeFailure, subprocess.TimeoutExpired) as error:
        print(f"OBS ingest smoke failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
