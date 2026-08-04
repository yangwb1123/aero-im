#!/usr/bin/env python3
"""Real-network RTMP, WHIP and SRT ingest smoke for Aero IM.

The script provisions an isolated account and one stream per selected protocol,
publishes deterministic FFmpeg lavfi media, waits for HLS output, and verifies
the resulting playlist with ffprobe. Stream keys and access tokens are never
printed. It expects a fully booted Aero server plus its Postgres/Redis/NATS
dependencies.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any


class SmokeFailure(RuntimeError):
    """A staging assertion failed."""


def api_json(
    base_url: str,
    method: str,
    path: str,
    *,
    body: dict[str, Any] | None = None,
    token: str | None = None,
    accepted: tuple[int, ...] = (200,),
) -> tuple[int, Any]:
    headers = {"accept": "application/json"}
    if token:
        headers["authorization"] = f"Bearer {token}"
    payload = None
    if body is not None:
        headers["content-type"] = "application/json"
        payload = json.dumps(body).encode()
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}{path}",
        method=method,
        data=payload,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            status = response.status
            raw = response.read()
    except urllib.error.HTTPError as error:
        status = error.code
        raw = error.read()
    except OSError as error:
        raise SmokeFailure(f"{method} {path} failed: {error}") from error
    if status not in accepted:
        detail = raw.decode(errors="replace")[:500]
        raise SmokeFailure(f"{method} {path}: HTTP {status}: {detail}")
    if not raw:
        return status, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError as error:
        raise SmokeFailure(f"{method} {path}: invalid JSON response") from error


def fetch_text(url: str) -> str | None:
    try:
        with urllib.request.urlopen(url, timeout=5) as response:
            return response.read().decode(errors="replace")
    except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
        return None


def metric_value(base_url: str, name: str) -> float:
    metrics = fetch_text(f"{base_url.rstrip('/')}/metrics")
    if metrics is None:
        raise SmokeFailure("metrics endpoint is unavailable")
    match = re.search(rf"^{re.escape(name)}(?:\{{[^}}]*\}})?\s+(\S+)$", metrics, re.M)
    if match is None:
        return 0.0
    try:
        return float(match.group(1))
    except ValueError as error:
        raise SmokeFailure(f"metric {name} is not numeric") from error


def append_query(url: str, values: dict[str, str]) -> str:
    parsed = urllib.parse.urlsplit(url)
    query = dict(urllib.parse.parse_qsl(parsed.query, keep_blank_values=True))
    query.update(values)
    return urllib.parse.urlunsplit(
        (
            parsed.scheme,
            parsed.netloc,
            parsed.path,
            urllib.parse.urlencode(query),
            parsed.fragment,
        )
    )


def ffmpeg_video_input() -> list[str]:
    return [
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
    ]


def ffmpeg_video_output(duration: int) -> list[str]:
    return [
        "-t",
        str(duration),
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-tune",
        "zerolatency",
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
        "-b:v",
        "700k",
    ]


def publisher_command(
    protocol: str,
    ingest_url: str,
    duration: int,
    ffmpeg: str,
    srt_ffmpeg: str,
    *,
    srt_passphrase: str | None,
    srt_pbkeylen: int,
    srt_kmrefreshrate: int | None,
    srt_kmpreannounce: int | None,
) -> list[str]:
    video_input = ffmpeg_video_input()
    video_output = ffmpeg_video_output(duration)
    if protocol == "whip":
        return [
            ffmpeg,
            *video_input,
            *video_output,
            "-an",
            "-f",
            "whip",
            ingest_url,
        ]

    audio_input = [
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=1000:sample_rate=48000",
    ]
    audio_output = [
        "-c:a",
        "aac",
        "-b:a",
        "96k",
        "-ar",
        "48000",
        "-ac",
        "2",
    ]
    if protocol == "rtmp":
        return [
            ffmpeg,
            *video_input,
            *audio_input,
            *video_output,
            *audio_output,
            "-f",
            "flv",
            ingest_url,
        ]
    if protocol == "srt":
        query = {
            "mode": "caller",
            "transtype": "live",
            "pkt_size": "1316",
            "latency": "120000",
        }
        if srt_passphrase is not None:
            query.update(
                {
                    "passphrase": srt_passphrase,
                    "pbkeylen": str(srt_pbkeylen),
                }
            )
        if srt_kmrefreshrate is not None:
            query["kmrefreshrate"] = str(srt_kmrefreshrate)
        if srt_kmpreannounce is not None:
            query["kmpreannounce"] = str(srt_kmpreannounce)
        target = append_query(
            ingest_url,
            query,
        )
        return [
            srt_ffmpeg,
            *video_input,
            *audio_input,
            *video_output,
            *audio_output,
            "-f",
            "mpegts",
            target,
        ]
    raise SmokeFailure(f"unsupported protocol: {protocol}")


def redact(text: str, secrets: tuple[str, ...]) -> str:
    for secret in secrets:
        if secret:
            text = text.replace(secret, "<redacted>")
    return text


def wait_for_hls(
    base_url: str,
    stream_id: str,
    token: str,
    process: subprocess.Popen[str],
    timeout: int,
) -> tuple[str, str]:
    hls_url = f"{base_url.rstrip('/')}/hls/{stream_id}/index.m3u8"
    deadline = time.monotonic() + timeout
    last_status = "unknown"
    process_exit_seen: float | None = None
    while time.monotonic() < deadline:
        playlist = fetch_text(hls_url)
        if playlist and "#EXTM3U" in playlist and ".ts" in playlist:
            return hls_url, playlist
        _, stream = api_json(
            base_url, "GET", f"/api/streams/{stream_id}", token=token
        )
        last_status = str(stream.get("status", "unknown"))
        if process.poll() is not None:
            process_exit_seen = process_exit_seen or time.monotonic()
            # Ingest finalizers write their last segment and playlist after the
            # publisher socket closes. Give that deterministic cleanup a short
            # grace period before reporting the original publisher failure.
            if time.monotonic() - process_exit_seen >= 3:
                break
        time.sleep(0.5)
    raise SmokeFailure(
        f"HLS playlist did not become ready within {timeout}s "
        f"(last stream status: {last_status})"
    )


def probe_hls(
    ffprobe: str,
    hls_url: str,
    protocol: str,
    expected_duration: int,
) -> tuple[list[str], float]:
    result = subprocess.run(
        [
            ffprobe,
            "-v",
            "error",
            "-show_entries",
            (
                "format=duration:"
                "stream=codec_type,codec_name,width,height,sample_rate,channels"
            ),
            "-of",
            "json",
            hls_url,
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=30,
        check=False,
    )
    if result.returncode != 0:
        raise SmokeFailure(f"ffprobe failed: {result.stdout[-1000:]}")
    try:
        streams = json.loads(result.stdout).get("streams", [])
    except json.JSONDecodeError as error:
        raise SmokeFailure("ffprobe returned invalid JSON") from error
    codecs = [
        f"{stream.get('codec_type')}:{stream.get('codec_name')}" for stream in streams
    ]
    if "video:h264" not in codecs:
        raise SmokeFailure(f"{protocol} HLS lacks H.264 video: {codecs}")
    if protocol in {"rtmp", "srt"} and "audio:aac" not in codecs:
        raise SmokeFailure(f"{protocol} HLS lacks AAC audio: {codecs}")
    try:
        duration = float(json.loads(result.stdout).get("format", {}).get("duration"))
    except (TypeError, ValueError) as error:
        raise SmokeFailure(f"{protocol} HLS lacks a valid duration") from error
    # A codec-only probe would let the original SRT zero-window regression pass:
    # it produced a valid but only ~0.4 s TS before libsrt hit its 5 s idle
    # timeout. Require a material fraction of the requested program and, for the
    # default 12 s run, more than that idle boundary.
    minimum_duration = max(2.0, min(expected_duration * 0.6, 6.0))
    if duration < minimum_duration:
        raise SmokeFailure(
            f"{protocol} HLS is truncated: {duration:.3f}s "
            f"(required at least {minimum_duration:.3f}s)"
        )
    return codecs, duration


def validate_ingest_endpoint(protocol: str, ingest_url: str) -> None:
    parsed = urllib.parse.urlsplit(ingest_url)
    expected_scheme = {"rtmp": "rtmp", "whip": "http", "srt": "srt"}[protocol]
    if parsed.scheme not in ({expected_scheme, "https"} if protocol == "whip" else {expected_scheme}):
        raise SmokeFailure(
            f"{protocol} create returned unexpected ingest scheme: {parsed.scheme!r}"
        )
    if not parsed.hostname or not parsed.port:
        raise SmokeFailure(f"{protocol} create returned ingest URL without host/port")


def run_protocol(
    args: argparse.Namespace,
    protocol: str,
    token: str,
    room_id: str,
) -> dict[str, Any]:
    _, stream = api_json(
        args.base_url,
        "POST",
        "/api/streams",
        token=token,
        body={
            "title": f"media-smoke-{protocol}-{time.time_ns()}",
            "protocol": protocol,
            "room_id": room_id,
        },
    )
    stream_id = str(stream["id"])
    stream_key = str(stream["stream_key"])
    ingest_url = str(stream["ingest_url"])
    validate_ingest_endpoint(protocol, ingest_url)
    rotation_before = None
    if protocol == "srt" and args.srt_kmrefreshrate is not None:
        rotation_before = metric_value(
            args.base_url, "aero_srt_key_rotations_total"
        )
    command = publisher_command(
        protocol,
        ingest_url,
        args.duration,
        args.ffmpeg,
        args.srt_ffmpeg,
        srt_passphrase=args.srt_passphrase,
        srt_pbkeylen=args.srt_pbkeylen,
        srt_kmrefreshrate=args.srt_kmrefreshrate,
        srt_kmpreannounce=args.srt_kmpreannounce,
    )
    process = subprocess.Popen(
        command,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        hls_url, playlist = wait_for_hls(
            args.base_url, stream_id, token, process, args.timeout
        )
        _, stderr = process.communicate(timeout=args.duration + 30)
    except BaseException:
        process.terminate()
        try:
            _, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            _, stderr = process.communicate(timeout=5)
        if stderr:
            print(
                redact(stderr[-1500:], (stream_key, args.srt_passphrase or "")),
                file=sys.stderr,
            )
        raise
    if process.returncode != 0:
        raise SmokeFailure(
            f"{protocol} FFmpeg exited {process.returncode}: "
            f"{redact(stderr[-1500:], (stream_key, args.srt_passphrase or ''))}"
        )
    final_playlist = fetch_text(hls_url) or playlist
    codecs, media_duration = probe_hls(
        args.ffprobe,
        hls_url,
        protocol,
        args.duration,
    )
    api_json(
        args.base_url,
        "POST",
        f"/api/streams/{stream_id}/end",
        token=token,
        accepted=(200, 409),
    )
    result = {
        "protocol": protocol,
        "stream_id": stream_id,
        "segments": sum(
            1 for line in final_playlist.splitlines() if line.endswith(".ts")
        ),
        "codecs": codecs,
        "duration_seconds": round(media_duration, 3),
    }
    if protocol == "srt":
        rotation_count = None
        if rotation_before is not None:
            rotation_count = int(
                metric_value(args.base_url, "aero_srt_key_rotations_total")
                - rotation_before
            )
            if rotation_count < args.srt_min_key_rotations:
                raise SmokeFailure(
                    "SRT stream completed without enough observed SEK switches: "
                    f"{rotation_count} < {args.srt_min_key_rotations}"
                )
        result.update(
            {
                "encrypted": args.srt_passphrase is not None,
                "kmrefreshrate": args.srt_kmrefreshrate,
                "kmpreannounce": args.srt_kmpreannounce,
                "key_rotations": rotation_count,
            }
        )
    return result


def executable(value: str) -> str:
    resolved = shutil.which(value)
    if resolved is None:
        raise argparse.ArgumentTypeError(f"executable not found: {value}")
    return resolved


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base-url",
        default=os.environ.get("AERO_HOST", "http://127.0.0.1:3030"),
    )
    parser.add_argument(
        "--protocol",
        dest="protocols",
        action="append",
        choices=("rtmp", "whip", "srt"),
        help="protocol to test; repeat to select several (default: all)",
    )
    parser.add_argument(
        "--ffmpeg",
        type=executable,
        default=shutil.which("ffmpeg"),
        help="FFmpeg with RTMP and WHIP muxers",
    )
    parser.add_argument(
        "--srt-ffmpeg",
        type=executable,
        default=shutil.which("/usr/bin/ffmpeg"),
        help="FFmpeg built with libsrt",
    )
    parser.add_argument(
        "--ffprobe",
        type=executable,
        default=shutil.which("ffprobe"),
    )
    parser.add_argument(
        "--srt-passphrase",
        default=os.environ.get("AERO_SRT_PASSPHRASE"),
        help="enable AES-128 SRT; defaults to AERO_SRT_PASSPHRASE",
    )
    parser.add_argument(
        "--srt-pbkeylen",
        type=int,
        choices=(16,),
        default=16,
        help="SRT SEK length; Aero currently supports AES-128 only",
    )
    parser.add_argument(
        "--srt-kmrefreshrate",
        type=int,
        help="packets between SRT SEK switches",
    )
    parser.add_argument(
        "--srt-kmpreannounce",
        type=int,
        help="packets before a switch to announce the next SRT SEK",
    )
    parser.add_argument(
        "--srt-min-key-rotations",
        type=int,
        default=2,
        help="minimum server-observed SEK switches when rotation is configured",
    )
    parser.add_argument("--duration", type=int, default=12)
    parser.add_argument("--timeout", type=int, default=25)
    args = parser.parse_args()
    if not args.ffmpeg or not args.srt_ffmpeg or not args.ffprobe:
        parser.error("ffmpeg, SRT-capable ffmpeg and ffprobe are required")
    if args.duration < 6 or args.duration > 120:
        parser.error("--duration must be between 6 and 120 seconds")
    if args.timeout < 5 or args.timeout > 180:
        parser.error("--timeout must be between 5 and 180 seconds")
    if args.srt_min_key_rotations < 0:
        parser.error("--srt-min-key-rotations cannot be negative")
    if args.srt_passphrase is not None:
        passphrase_len = len(args.srt_passphrase.encode())
        if passphrase_len < 10 or passphrase_len > 64:
            parser.error("--srt-passphrase must encode to 10..64 bytes")
    rotation_values = (args.srt_kmrefreshrate, args.srt_kmpreannounce)
    if any(value is not None for value in rotation_values):
        if args.srt_passphrase is None:
            parser.error("SRT key rotation requires --srt-passphrase")
        if any(value is None for value in rotation_values):
            parser.error(
                "--srt-kmrefreshrate and --srt-kmpreannounce must be set together"
            )
        if args.srt_kmrefreshrate <= 1:
            parser.error("--srt-kmrefreshrate must be greater than 1")
        if not 0 < args.srt_kmpreannounce < args.srt_kmrefreshrate:
            parser.error(
                "--srt-kmpreannounce must be between 1 and the refresh rate"
            )
    args.protocols = args.protocols or ["rtmp", "whip", "srt"]
    args.base_url = args.base_url.rstrip("/")
    return args


def main() -> int:
    args = parse_args()
    suffix = time.time_ns()
    _, registered = api_json(
        args.base_url,
        "POST",
        "/api/auth/register",
        body={
            "email": f"media-smoke-{suffix}@aero.test",
            "password": "local_media_smoke_password_1234",
            "display_name": "Media Smoke",
        },
    )
    token = str(registered["access_token"])
    _, room = api_json(
        args.base_url,
        "POST",
        "/api/rooms",
        token=token,
        body={"kind": "group", "name": f"media-smoke-{suffix}"},
    )
    results = [
        run_protocol(args, protocol, token, str(room["id"]))
        for protocol in args.protocols
    ]
    print(json.dumps({"ok": True, "results": results}, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (SmokeFailure, subprocess.TimeoutExpired) as error:
        print(f"media ingest smoke failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
