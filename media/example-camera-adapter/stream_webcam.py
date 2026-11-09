#!/usr/bin/env python3
"""Send low-latency webcam H.264 access units to Fleetly Media."""

from __future__ import annotations

import argparse
import ctypes
import errno
import re
import signal
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path
from typing import BinaryIO, Iterator

PROTOCOL_VERSION = 1
CODEC_H264 = 1
HELLO = 0x01
VIDEO_FRAME = 0x02
HELLO_ACK = 0x80
KEYFRAME_REQUEST = 0x81
RATE_CONTROL = 0x82
MAX_ACCESS_UNIT_BYTES = 4 * 1024 * 1024 - 14  # Include VIDEO_FRAME fields in protocol limit.
MULTIPART_BOUNDARY = b"--fleetly"
NAL_START = re.compile(b"\x00\x00(?:\x00)?\x01([\x00-\xff])")


class PassthroughUnavailable(RuntimeError):
    """The camera's encoded stream cannot satisfy the local media contract."""


def h264_probe(config: argparse.Namespace, initial: bytes) -> bytes:
    if len(initial) != 46:
        raise RuntimeError("unsupported UVC H.264 configuration length")
    probe = bytearray(initial)
    struct.pack_into("<IIH", probe, 0, 10_000_000 // config.fps, config.bitrate, 0xA80F)
    struct.pack_into("<HH", probe, 12, config.width, config.height)
    struct.pack_into("<HH", probe, 20, 0x4240, 1000)
    probe[28], probe[29] = 1, 1  # realtime, CBR; fixed-rate flag is not universal
    probe[34], probe[35], probe[37] = 0, 0, 0  # Annex-B, CAVLC, no reordering
    return bytes(probe)


def configure_windows_h264(config: argparse.Namespace) -> None:
    """Use the same standard UVC controls through Windows Kernel Streaming."""
    from ctypes import wintypes
    device = config.device
    if not device.startswith("@device_pnp_"):
        listing = subprocess.run([config.ffmpeg, "-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"],
                                 capture_output=True, timeout=3, encoding="utf-8", errors="replace")
        selected, matches = False, []
        for line in listing.stderr.splitlines():
            if "(video)" in line:
                selected = f'"{device}" (video)' in line
            elif "Alternative name" in line and selected:
                matches.append(line.split('Alternative name "', 1)[1].rsplit('"', 1)[0])
                selected = False
        if len(matches) != 1:
            raise RuntimeError("use the unique DirectShow Alternative name as --device")
        device = matches[0]
    path = re.sub(r"\{65e8773d-8f56-11d0-a3b9-00a0c9223196\}",
                  "{6994ad05-93ef-11d0-a3cc-00a0c9223196}", device.removeprefix("@device_pnp_"), flags=re.IGNORECASE)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
                                  wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.DeviceIoControl.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.c_void_p, wintypes.DWORD,
                                      ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    kernel.DeviceIoControl.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.CreateFileW(path, 0xC0000000, 3, None, 3, 0, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        def query(guid: str, selector: int, data: bytes, node: int | None = None, write=False) -> bytes:
            header = uuid.UUID(guid).bytes_le + struct.pack("<II", selector, (2 if write else 1) | (0x10000000 if node is not None else 0))
            if node is not None:
                header += struct.pack("<II", node, 0)
            request = ctypes.create_string_buffer(header)
            buffer = ctypes.create_string_buffer(data, len(data))
            returned = wintypes.DWORD()
            # KS property data uses the output buffer, also for SET requests.
            if not kernel.DeviceIoControl(handle, 0x2F0003, request, len(header), buffer, len(data), ctypes.byref(returned), None):
                raise ctypes.WinError(ctypes.get_last_error())
            return buffer.raw if write else buffer.raw[:returned.value]
        nodes = query("720d4ac0-7533-11d0-a5d6-28db04c10000", 1, bytes(4096))
        if len(nodes) < 8:
            raise RuntimeError("invalid camera topology")
        count = struct.unpack_from("<I", nodes, 4)[0]
        if count > 64 or len(nodes) < 8 + count * 16:
            raise RuntimeError("invalid camera topology size")
        guid = "a29e7641-de04-47e3-8b2b-f4341aff003b"
        for node in range(count):
            try:
                initial = query(guid, 1, bytes(46), node)
            except OSError as error:
                if error.winerror in (1, 50, 1170):
                    continue  # Node does not expose the standard H.264 extension.
                raise
            query(guid, 1, h264_probe(config, initial), node, write=True)
            accepted = query(guid, 1, bytes(46), node)
            if len(accepted) != 46 or struct.unpack_from("<HH", accepted, 12) != (config.width, config.height):
                raise RuntimeError("camera cannot provide the requested H.264 dimensions")
            query(guid, 2, accepted, node, write=True)
            print("[camera] configured Windows UVC H.264: constrained baseline, one-second keyframes", flush=True)
            return
    finally:
        kernel.CloseHandle(handle)


def uvc_h264_unit(device: str) -> int | None:
    """Discover the standard UVC H.264 extension, independent of camera model."""
    path = (Path("/sys/class/video4linux") / Path(device).resolve().name / "device").resolve()
    guid = bytes.fromhex("41769ea204dee3478b2bf4341aff003b")
    for parent in (path, *path.parents):
        if not (parent / "descriptors").exists():
            continue
        descriptors = (parent / "descriptors").read_bytes()
        index = 0
        while index + 2 <= len(descriptors) and descriptors[index] >= 2:
            descriptor = descriptors[index:index + descriptors[index]]
            if len(descriptor) >= 20 and descriptor[1:3] == b"\x24\x06" and descriptor[4:20] == guid:
                return descriptor[3]
            index += descriptors[index]
        break
    return None


def configure_uvc_h264(config: argparse.Namespace) -> None:
    # Standard UVC H.264 payload 1.0 controls, also used by GStreamer's
    # uvch264src. No Logitech-specific IDs, mappings, or privileged ioctls.
    import fcntl

    unit = uvc_h264_unit(config.device)
    if unit is None:
        return  # Other cameras can still pass validation without these controls.

    class Query(ctypes.Structure):
        _fields_ = [("unit", ctypes.c_uint8), ("selector", ctypes.c_uint8),
                    ("query", ctypes.c_uint8), ("size", ctypes.c_uint16),
                    ("data", ctypes.c_void_p)]

    with open(config.device, "r+b", buffering=0) as device:
        def query(selector: int, request: int, data: bytes) -> bytes:
            buffer = ctypes.create_string_buffer(data, len(data))
            control = Query(unit, selector, request, len(data), ctypes.addressof(buffer))
            ioctl = 0xC0007521 | (ctypes.sizeof(Query) << 16)  # UVCIOC_CTRL_QUERY
            fcntl.ioctl(device.fileno(), ioctl, control)
            return buffer.raw

        length = struct.unpack("<H", query(1, 0x85, bytes(2)))[0]  # GET_LEN
        if length != 46:
            raise RuntimeError(f"unsupported UVC H.264 configuration length {length}")
        # A previous rejected probe may leave GET_CUR with zero dimensions.
        # Start from GET_DEF for each new capture/format negotiation.
        query(1, 1, h264_probe(config, query(1, 0x87, bytes(length))))  # GET_DEF, PROBE SET_CUR
        accepted = query(1, 0x81, bytes(length))
        if struct.unpack_from("<HH", accepted, 12) != (config.width, config.height):
            raise RuntimeError("camera cannot provide the requested H.264 dimensions")
        query(2, 1, accepted)  # COMMIT SET_CUR
        print("[camera] configured UVC H.264: constrained baseline, one-second keyframes", flush=True)


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Example Fleetly webcam source adapter")
    parser.add_argument("--mode", choices=("auto", "software", "hardware-h264"), default="auto")
    parser.add_argument("--device", default="USB Video Device")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=7002)
    parser.add_argument("--width", type=int)
    parser.add_argument("--height", type=int)
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument("--bitrate", type=int, default=2_000_000)
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--gstreamer")
    parser.add_argument("--reconnect-delay", type=float, default=1.0)
    parser.add_argument(
        "--stats-interval",
        type=float,
        default=0.0,
        help="print frame and keyframe diagnostics every N seconds (disabled by default)",
    )
    config = parser.parse_args()
    if config.width is None:
        config.width = 1280 if config.mode == "hardware-h264" else 640
    if config.height is None:
        config.height = 720 if config.mode == "hardware-h264" else 360
    if not (1 <= config.fps <= 60 and 0 < config.width <= 3840 and 0 < config.height <= 2160
            and config.width % 2 == 0 and config.height % 2 == 0 and 0 < config.bitrate <= 50_000_000):
        parser.error("use even dimensions up to 3840x2160, 1-60 fps, and bitrate 1-50000000")
    return config


def ffmpeg_command(config: argparse.Namespace) -> list[str]:
    common = [config.ffmpeg, "-hide_banner", "-loglevel", "warning", "-fflags", "nobuffer", "-flags", "low_delay"]
    if sys.platform == "win32":
        source = ["-f", "dshow", "-video_size", f"{config.width}x{config.height}", "-framerate", str(config.fps), "-i", f"video={config.device}"]
    elif sys.platform.startswith("linux"):
        # Raw YUYV can be limited to 5 fps at 1080p over USB. Prefer the
        # camera's compressed MJPEG input when available for software fallback.
        try:
            formats = subprocess.run([config.ffmpeg, "-hide_banner", "-f", "v4l2", "-list_formats", "all", "-i", config.device],
                                     capture_output=True, timeout=3)
            compressed = ["-input_format", "mjpeg"] if b"mjpeg" in formats.stderr.lower() else []
        except (OSError, subprocess.TimeoutExpired):
            compressed = []
        source = ["-f", "v4l2", *compressed, "-video_size", f"{config.width}x{config.height}", "-framerate", str(config.fps), "-i", config.device]
    else:
        raise RuntimeError("the software adapter supports Windows and Linux")

    bitrate_kbps = max(1, config.bitrate // 1000)
    return common + source + [
        "-an", "-vf", f"scale={config.width}:{config.height},format=yuv420p",
        "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
        "-profile:v", "baseline",
        "-b:v", f"{bitrate_kbps}k", "-maxrate", f"{bitrate_kbps}k", "-bufsize", f"{max(1, bitrate_kbps // 2)}k",
        "-g", str(config.fps), "-keyint_min", str(config.fps), "-sc_threshold", "0", "-bf", "0",
        "-x264-params", "aud=1:repeat-headers=1:bframes=0:sync-lookahead=0:rc-lookahead=0",
        "-f", "h264", "pipe:1",
    ]


def passthrough_command(config: argparse.Namespace) -> list[str]:
    if not sys.platform.startswith("linux"):
        return gstreamer_command(config)
    return [
        config.ffmpeg, "-hide_banner", "-loglevel", "warning",
        "-fflags", "nobuffer", "-flags", "low_delay",
        "-f", "v4l2", "-input_format", "h264",
        "-video_size", f"{config.width}x{config.height}",
        "-framerate", str(config.fps), "-i", config.device,
        "-an", "-c:v", "copy", "-bsf:v", "h264_metadata=aud=insert",
        "-flush_packets", "1", "-f", "h264", "pipe:1",
    ]


def gstreamer_executable(config: argparse.Namespace) -> str:
    if config.gstreamer:
        return config.gstreamer
    found = shutil.which("gst-launch-1.0")
    if found:
        return found
    windows_default = Path(r"C:\Program Files\gstreamer\1.0\msvc_x86_64\bin\gst-launch-1.0.exe")
    if windows_default.exists():
        return str(windows_default)
    raise RuntimeError("gst-launch-1.0 was not found; use --gstreamer to specify it")


def gstreamer_command(config: argparse.Namespace) -> list[str]:
    if sys.platform != "win32":
        raise RuntimeError("hardware-h264 mode currently targets the Windows C920 test camera")
    # DirectShow's alternative name and Kernel Streaming's capture path refer
    # to the same device instance through different interface categories.
    # Keep that instance when multiple cameras share a friendly name.
    device = config.device
    if device.startswith("@device_pnp_"):
        device = device.removeprefix("@device_pnp_")
        device = re.sub(r"\{65e8773d-8f56-11d0-a3b9-00a0c9223196\}",
                        "{6994ad05-93ef-11d0-a3cc-00a0c9223196}", device, flags=re.IGNORECASE)
        property_name = "device-path"
    else:
        property_name = "device-name"
    # gst-launch parses a pipeline string after joining argv; preserve literal
    # backslashes and spaces in names/paths rather than losing them to parsing.
    device = device.replace("\\", "\\\\").replace('"', '\\"')
    return [
        gstreamer_executable(config), "-q",
        "ksvideosrc", f'{property_name}="{device}"', "!",
        f"video/x-raw,format=H264,width={config.width},height={config.height},framerate={config.fps}/1", "!",
        "capssetter", "caps=video/x-h264,stream-format=byte-stream,alignment=au", "replace=true", "!",
        "multipartmux", "boundary=fleetly", "!", "fdsink", "fd=1", "sync=false",
    ]


def send_message(connection: socket.socket, message_type: int, payload: bytes) -> None:
    body = bytes([message_type]) + payload
    connection.sendall(struct.pack(">I", len(body)) + body)


def receive_exact(stream: socket.socket | BinaryIO, length: int) -> bytes:
    output = bytearray()
    while len(output) < length:
        chunk = stream.recv(length - len(output)) if isinstance(stream, socket.socket) else stream.read(length - len(output))
        if not chunk:
            raise ConnectionError("stream ended unexpectedly")
        output.extend(chunk)
    return bytes(output)


def receive_message(connection: socket.socket) -> tuple[int, bytes]:
    length = struct.unpack(">I", receive_exact(connection, 4))[0]
    if length < 1 or length > 1024:
        raise ConnectionError(f"invalid server message length {length}")
    message = receive_exact(connection, length)
    return message[0], message[1:]


def stop_source(process: subprocess.Popen[bytes]) -> None:
    process.terminate()
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def start_control_reader(connection: socket.socket, stopped: threading.Event,
                         process: subprocess.Popen[bytes]) -> threading.Thread:
    def read_controls() -> None:
        try:
            while not stopped.is_set():
                message_type, payload = receive_message(connection)
                if message_type == KEYFRAME_REQUEST:
                    print("[fleetly-media] requested a keyframe; this example uses periodic IDRs")
                elif message_type == RATE_CONTROL and len(payload) == 6:
                    bitrate, fps = struct.unpack(">IH", payload)
                    print(f"[fleetly-media] requested {bitrate} bit/s at {fps} fps; this example is fixed-rate")
                else:
                    print(f"[fleetly-media] ignored server message 0x{message_type:02x}")
        except (ConnectionError, OSError) as error:
            if not stopped.is_set():
                print(f"[fleetly-media] control connection ended: {error}")
        finally:
            stopped.set()
            # Wake the blocked frame reader even when the camera produces nothing.
            stop_source(process)

    reader = threading.Thread(target=read_controls, daemon=True)
    reader.start()
    return reader


def nal_types(access_unit: bytes) -> set[int]:
    return {match[1][0] & 0x1F for match in NAL_START.finditer(access_unit)}


def h264_profile_level_id(access_unit: bytes) -> str | None:
    for match in NAL_START.finditer(access_unit):
        if match[1][0] & 0x1F == 7 and match.end() + 3 <= len(access_unit):
            return access_unit[match.end():match.end() + 3].hex()
    return None


def aud_positions(data: bytearray) -> list[int]:
    return [match.start() for match in NAL_START.finditer(data) if match[1][0] & 0x1F == 9]


def ffmpeg_access_units(stdout: BinaryIO) -> Iterator[bytes]:
    buffer = bytearray()
    while chunk := stdout.read1(64 * 1024):
        buffer.extend(chunk)
        positions = aud_positions(buffer)
        for start, end in zip(positions, positions[1:]):
            if end - start > MAX_ACCESS_UNIT_BYTES:
                raise RuntimeError("FFmpeg produced an oversized access unit")
            yield bytes(buffer[start:end])
        if len(positions) >= 2:
            del buffer[:positions[-1]]
        if len(buffer) > MAX_ACCESS_UNIT_BYTES:
            raise RuntimeError("FFmpeg produced an oversized or unframed access unit")
    positions = aud_positions(buffer)
    if positions:
        yield bytes(buffer[positions[0]:])


def multipart_access_units(stdout: BinaryIO) -> Iterator[bytes]:
    while True:
        line = stdout.readline()
        if not line:
            return
        if line.rstrip(b"\r\n") != MULTIPART_BOUNDARY:
            continue
        headers: dict[bytes, bytes] = {}
        while True:
            line = stdout.readline()
            if not line:
                raise ConnectionError("GStreamer multipart headers ended unexpectedly")
            if line in (b"\r\n", b"\n"):
                break
            name, separator, value = line.partition(b":")
            if not separator:
                raise RuntimeError("invalid GStreamer multipart header")
            headers[name.strip().lower()] = value.strip()
        try:
            length = int(headers[b"content-length"])
        except (KeyError, ValueError) as error:
            raise RuntimeError("GStreamer multipart frame lacks a valid Content-Length") from error
        if length < 1 or length > MAX_ACCESS_UNIT_BYTES:
            raise RuntimeError(f"GStreamer produced invalid access-unit length {length}")
        yield receive_exact(stdout, length)


def source_process(config: argparse.Namespace) -> tuple[subprocess.Popen[bytes], Iterator[bytes]]:
    if sys.platform.startswith("linux"):
        configure_linux_framerate(config.device)
        if config.mode == "hardware-h264":
            configure_uvc_h264(config)
    elif sys.platform == "win32" and config.mode == "hardware-h264":
        configure_windows_h264(config)
    command = passthrough_command(config) if config.mode == "hardware-h264" else ffmpeg_command(config)
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
    assert process.stdout is not None
    frames = multipart_access_units(process.stdout) if config.mode == "hardware-h264" and sys.platform == "win32" else ffmpeg_access_units(process.stdout)
    return process, frames


def configure_linux_framerate(device: str) -> None:
    import fcntl

    # Keep auto exposure from lowering the explicitly requested frame rate.
    # This is the standard V4L2 control, not a camera-model-specific setting.
    try:
        with open(device, "r+b", buffering=0) as camera:
            fcntl.ioctl(camera.fileno(), 0xC008561C, struct.pack("=Ii", 0x009A0903, 0))
    except OSError as error:
        if error.errno not in (errno.EINVAL, errno.ENOTTY):
            raise
        print("[camera] device has no fixed-framerate exposure control", flush=True)


def sps_dimensions(access_unit: bytes) -> tuple[int, int]:
    starts = list(NAL_START.finditer(access_unit))
    for index, match in enumerate(starts):
        if match[1][0] & 31 != 7:
            continue
        end = starts[index + 1].start() if index + 1 < len(starts) else len(access_unit)
        sps = access_unit[match.end():end]
        if len(sps) < 4 or len(sps) > 256 or sps[0] != 66 or not sps[1] & 0x40:
            raise RuntimeError("camera H.264 must use constrained-baseline profile")
        bits = "".join(f"{byte:08b}" for byte in sps[3:].replace(b"\x00\x00\x03", b"\x00\x00"))
        position = 0

        def read(count: int) -> int:
            nonlocal position
            if position + count > len(bits):
                raise RuntimeError("truncated H.264 SPS")
            value = int(bits[position:position + count], 2) if count else 0
            position += count
            return value

        def ue() -> int:
            zeros = 0
            while read(1) == 0:
                zeros += 1
                if zeros > 16:
                    raise RuntimeError("invalid H.264 SPS value")
            return (1 << zeros) - 1 + read(zeros)

        ue()  # SPS id
        ue()  # log2_max_frame_num_minus4
        poc = ue()
        if poc == 0:
            ue()
        elif poc == 1:
            read(1)
            ue(), ue()  # signed Exp-Golomb values have the same bit length
            for _ in range(ue()):
                ue()
        elif poc != 2:
            raise RuntimeError("invalid H.264 picture order count")
        ue(), read(1)  # reference frames, gaps flag
        width, height = (ue() + 1) * 16, (ue() + 1) * 16
        if read(1) != 1:
            raise RuntimeError("interlaced camera H.264 is not supported")
        read(1)
        if read(1):
            left, right, top, bottom = ue(), ue(), ue(), ue()
            width -= 2 * (left + right)
            height -= 2 * (top + bottom)
        return width, height
    raise RuntimeError("camera keyframe has no SPS")


def start_source_watchdog(process: subprocess.Popen[bytes], stopped: threading.Event,
                          deadline: list[float], timed_out: threading.Event) -> None:
    def watch() -> None:
        while not stopped.wait(0.25):
            if time.monotonic() > deadline[0]:
                timed_out.set()
                stop_source(process)  # Wake stdout.read1 even for a frozen source.
                return

    threading.Thread(target=watch, daemon=True).start()


def frame_timestamp_us(start_us: int, frame_index: int, fps: int) -> int:
    return start_us + (frame_index * 1_000_000) // fps


def stream_once(config: argparse.Namespace) -> None:
    try:
        connection = socket.create_connection((config.host, config.port), timeout=5)
    except ConnectionRefusedError:
        # No authorized publication is normal while nobody is watching.
        return
    with connection:
        hello = struct.pack(">BBHHHIB", PROTOCOL_VERSION, CODEC_H264, config.width, config.height, config.fps, config.bitrate, 0)
        send_message(connection, HELLO, hello)
        message_type, payload = receive_message(connection)
        if message_type != HELLO_ACK or payload != bytes([PROTOCOL_VERSION]):
            raise ConnectionError("fleetly-media rejected the adapter HELLO")
        connection.settimeout(None)
        print(f"Connected to fleetly-media at {config.host}:{config.port}", flush=True)

        stopped = threading.Event()
        capture = argparse.Namespace(**vars(config))
        capture.mode = "hardware-h264" if config.mode == "auto" else config.mode
        hardware = capture.mode == "hardware-h264"
        try:
            process, frames = source_process(capture)
        except (OSError, RuntimeError) as error:
            if hardware:
                raise PassthroughUnavailable(str(error)) from error
            raise
        controls = start_control_reader(connection, stopped, process)
        deadline = [time.monotonic() + 5]
        timed_out = threading.Event()
        start_source_watchdog(process, stopped, deadline, timed_out)
        print(f"[camera] trying {capture.mode}: {config.width}x{config.height}@{config.fps}", flush=True)
        clock_start_us = time.monotonic_ns() // 1000
        stats_started = time.monotonic()
        stats_frames = 0
        stats_bytes = 0
        stats_keyframes = 0
        previous_keyframe_at: float | None = None
        frame_index = 0
        try:
            for access_unit in frames:
                if stopped.is_set():
                    break
                if timed_out.is_set():
                    raise RuntimeError("camera source stalled")
                now = time.monotonic()
                deadline[0] = now + 3
                types = nal_types(access_unit)
                keyframe = 5 in types
                if keyframe and not {7, 8}.issubset(types):
                    raise RuntimeError("source keyframe did not include SPS and PPS")
                if hardware:
                    if previous_keyframe_at is None and not keyframe:
                        if now - stats_started > 3:
                            raise RuntimeError("camera did not provide an initial keyframe")
                        continue
                    if previous_keyframe_at is not None and now - previous_keyframe_at > 2.5:
                        raise RuntimeError("camera keyframes are more than 2.5 seconds apart")
                    if keyframe and sps_dimensions(access_unit) != (config.width, config.height):
                        raise RuntimeError("camera H.264 dimensions differ from requested dimensions")
                if frame_index == 0:
                    path = "camera H.264 passthrough (no software encoder)" if hardware else "software H.264 encoding (libx264)"
                    print(f"[camera] using {path}", flush=True)
                stats_frames += 1
                stats_bytes += len(access_unit)
                if keyframe:
                    stats_keyframes += 1
                    spacing = "first"
                    if previous_keyframe_at is not None:
                        spacing = f"{now - previous_keyframe_at:.2f}s since previous"
                    previous_keyframe_at = now
                    if config.stats_interval > 0:
                        profile_level_id = h264_profile_level_id(access_unit) or "unknown"
                        print(
                            f"[camera] keyframe #{stats_keyframes}: frame={frame_index + 1} "
                            f"bytes={len(access_unit)} profile-level-id={profile_level_id} ({spacing})",
                            flush=True,
                        )
                elapsed = now - stats_started
                if config.stats_interval > 0 and elapsed >= config.stats_interval:
                    keyframe_age = "never"
                    if previous_keyframe_at is not None:
                        keyframe_age = f"{now - previous_keyframe_at:.2f}s"
                    print(
                        f"[camera] {stats_frames / elapsed:.1f} fps, "
                        f"{stats_bytes * 8 / elapsed / 1_000_000:.2f} Mbit/s, "
                        f"keyframes={stats_keyframes}, last_keyframe_age={keyframe_age}",
                        flush=True,
                    )
                    stats_started = now
                    stats_frames = 0
                    stats_bytes = 0
                    stats_keyframes = 0
                timestamp_us = frame_timestamp_us(clock_start_us, frame_index, config.fps)
                frame = struct.pack(">BHHQ", 1 if keyframe else 0, config.width, config.height, timestamp_us) + access_unit
                send_message(connection, VIDEO_FRAME, frame)
                frame_index += 1
            if not stopped.is_set():
                reason = "camera source stalled" if timed_out.is_set() else "camera source ended"
                raise RuntimeError(reason)
        except RuntimeError as error:
            if hardware and not stopped.is_set():
                raise PassthroughUnavailable(str(error)) from error
            if not stopped.is_set():
                raise
        finally:
            stopped.set()
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stop_source(process)
            controls.join(timeout=1)
            process.stdout.close()


def main() -> None:
    # Background shells may inherit SIGINT as ignored. Always release capture
    # through stream_once's finally block on Ctrl+C or service termination.
    signal.signal(signal.SIGINT, signal.default_int_handler)
    signal.signal(signal.SIGTERM, signal.default_int_handler)
    config = arguments()
    while True:
        try:
            stream_once(config)
        except KeyboardInterrupt:
            return
        except PassthroughUnavailable as error:
            if config.mode == "auto":
                print(f"[camera] H.264 passthrough unavailable: {error}; falling back to software", flush=True)
                config.mode = "software"  # Avoid repeated failing probes on every reconnect.
            else:
                print(f"Camera adapter stopped: {error}", flush=True)
        except (ConnectionError, OSError, RuntimeError) as error:
            print(f"Camera adapter stopped: {error}")
        try:
            time.sleep(config.reconnect_delay)
        except KeyboardInterrupt:
            return


if __name__ == "__main__":
    main()
