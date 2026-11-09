import importlib.util
import io
import argparse
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import tempfile
import unittest
from unittest.mock import patch, MagicMock
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("stream_webcam.py")
SPEC = importlib.util.spec_from_file_location("stream_webcam", MODULE_PATH)
assert SPEC and SPEC.loader
ADAPTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ADAPTER)


def baseline_keyframe(width=640, height=480):
    def ue(value):
        encoded = f"{value + 1:b}"
        return "0" * (len(encoded) - 1) + encoded
    bottom = ((height + 15) // 16 * 16 - height) // 2
    bits = ue(0) * 4 + ue(1) + "0" + ue(width // 16 - 1) + ue((height + 15) // 16 - 1) + "11"
    bits += "1" + ue(0) * 3 + ue(bottom) if bottom else "0"
    bits += "1"
    bits += "0" * (-len(bits) % 8)
    sps = b"\x00\x00\x00\x01\x67\x42\xc0\x28" + int(bits, 2).to_bytes(len(bits) // 8, 'big')
    return b"\x00\x00\x00\x01\x09\xf0" + sps + b"\x00\x00\x01\x68\xce\x00\x00\x01\x65\xaa"


class AccessUnitTests(unittest.TestCase):
    def test_reads_profile_level_id_from_sps(self) -> None:
        access_unit = b"\x00\x00\x00\x01\x67\x64\x00\x1f\xaa\xbb\x00\x00\x01\x65\xcc"
        self.assertEqual(ADAPTER.h264_profile_level_id(access_unit), "64001f")

    def test_missing_sps_has_no_profile_level_id(self) -> None:
        self.assertIsNone(ADAPTER.h264_profile_level_id(b"\x00\x00\x01\x65\xcc"))

    def test_frame_clock_is_strictly_monotonic(self) -> None:
        timestamps = [
            ADAPTER.frame_timestamp_us(1_000_000, index, 30) for index in range(120)
        ]
        self.assertTrue(all(left < right for left, right in zip(timestamps, timestamps[1:])))
        self.assertEqual(timestamps[30], 2_000_000)

    def test_four_byte_aud_is_not_counted_twice(self) -> None:
        data = bytearray(b"\x00\x00\x00\x01\x09\xf0payload")
        self.assertEqual(ADAPTER.aud_positions(data), [0])

    def test_annex_b_frames_survive_every_pipe_split(self):
        first = baseline_keyframe()
        second = b"\x00\x00\x01\x09\xf0\x00\x00\x01\x61delta"
        third = b"\x00\x00\x00\x01\x09\xf0\x00\x00\x01\x61last"
        encoded = b"leading-data" + first + second + third
        class Chunks:
            def __init__(self, split):
                self.chunks = iter((encoded[:split], encoded[split:]))
            def read1(self, _count):
                return next(self.chunks, b'')
        for split in range(1, len(encoded)):
            with self.subTest(split=split):
                self.assertEqual(list(ADAPTER.ffmpeg_access_units(Chunks(split))), [first, second, third])

    def test_validates_cropped_1080p_and_480p_sps(self):
        for width, height in ((640, 480), (1920, 1080)):
            self.assertEqual(ADAPTER.sps_dimensions(baseline_keyframe(width, height)), (width, height))

    def test_rejects_unsupported_profile_and_truncated_sps(self):
        bad = baseline_keyframe().replace(b'\x67\x42\xc0', b'\x67\x64\x00')
        with self.assertRaisesRegex(RuntimeError, 'constrained-baseline'):
            ADAPTER.sps_dimensions(bad)
        with self.assertRaises(RuntimeError):
            ADAPTER.sps_dimensions(b'\x00\x00\x01\x67\x42\xc0\x28\x00')

    def test_oversized_unframed_stream_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'oversized'):
            list(ADAPTER.ffmpeg_access_units(io.BytesIO(b'x' * (ADAPTER.MAX_ACCESS_UNIT_BYTES + 1))))

    def test_frame_delimiter_does_not_hide_oversized_access_unit(self):
        start = b'\x00\x00\x00\x01\x09\xf0'
        class SingleChunk:
            def read1(self, _count):
                return start + b'x' * ADAPTER.MAX_ACCESS_UNIT_BYTES + start
        with self.assertRaisesRegex(RuntimeError, 'oversized'):
            next(ADAPTER.ffmpeg_access_units(SingleChunk()))

    def test_multipart_preserves_access_unit_boundaries(self) -> None:
        first = b"\x00\x00\x00\x01\x67one"
        second = b"\x00\x00\x00\x01\x61two"
        stream = io.BytesIO(
            b"--fleetly\r\nContent-Type: video/x-raw\r\nContent-Length: "
            + str(len(first)).encode()
            + b"\r\n\r\n"
            + first
            + b"\r\n--fleetly\r\nContent-Length: "
            + str(len(second)).encode()
            + b"\r\n\r\n"
            + second
        )
        self.assertEqual(list(ADAPTER.multipart_access_units(stream)), [first, second])


class CaptureShutdownTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith('linux'), 'POSIX source shutdown is tested on Linux')
    def test_sigterm_releases_blocked_capture_and_media_socket(self):
        with tempfile.TemporaryDirectory() as directory, socket.socket() as server:
            server.bind(('127.0.0.1', 0))
            server.listen(1)
            server.settimeout(3)
            pid_file = Path(directory) / 'capture.pid'
            code = f'''
import importlib.util, subprocess, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('adapter', {str(MODULE_PATH)!r})
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
def source(config):
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], stdout=subprocess.PIPE)
    Path({str(pid_file)!r}).write_text(str(child.pid))
    return child, adapter.ffmpeg_access_units(child.stdout)
adapter.source_process = source
sys.argv = ['stream_webcam.py', '--mode', 'software', '--port', {str(server.getsockname()[1])!r}]
adapter.main()
'''
            helper = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            capture_pid = None
            try:
                with server.accept()[0] as connection:
                    connection.settimeout(3)
                    self.assertEqual(ADAPTER.receive_message(connection)[0], ADAPTER.HELLO)
                    ADAPTER.send_message(connection, ADAPTER.HELLO_ACK, bytes([1]))
                    deadline = time.monotonic() + 2
                    while not pid_file.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    capture_pid = int(pid_file.read_text())
                    helper.send_signal(signal.SIGTERM)
                    self.assertEqual(connection.recv(1), b'')
                self.assertEqual(helper.wait(timeout=3), 0)
                with self.assertRaises(ProcessLookupError):
                    os.kill(capture_pid, 0)
            finally:
                if helper.poll() is None:
                    helper.kill()
                helper.wait(timeout=3)
                helper.stdout.close()
                helper.stderr.close()
                if capture_pid is not None:
                    try:
                        os.kill(capture_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass

    def test_watchdog_releases_a_frozen_source(self):
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], stdout=subprocess.PIPE)
        stopped, timed_out = threading.Event(), threading.Event()
        try:
            ADAPTER.start_source_watchdog(child, stopped, [time.monotonic() - 1], timed_out)
            child.wait(timeout=2)
            self.assertTrue(timed_out.is_set())
        finally:
            stopped.set()
            if child.poll() is None:
                child.kill()
            child.wait(timeout=3)
            child.stdout.close()

    def test_closed_media_connection_releases_a_source_that_produces_no_frames(self) -> None:
        # A real blocked stdout pipe reproduces a frozen camera, without hardware.
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], stdout=subprocess.PIPE)
        source_started = threading.Event()
        failures = []
        with socket.socket() as server:
            server.bind(('127.0.0.1', 0))
            server.listen(1)
            server.settimeout(3)
            config = argparse.Namespace(host='127.0.0.1', port=server.getsockname()[1],
                width=640, height=360, fps=30, bitrate=2_000_000, device='frozen-fixture',
                mode='software', stats_interval=0)
            def source(_config):
                source_started.set()
                return child, ADAPTER.ffmpeg_access_units(child.stdout)
            def stream():
                try:
                    ADAPTER.stream_once(config)
                except ConnectionError:
                    pass  # Terminated capture closes its stdout pipe.
                except Exception as error:
                    failures.append(error)
            with patch.object(ADAPTER, 'source_process', side_effect=source):
                runner = threading.Thread(target=stream, daemon=True)
                runner.start()
                try:
                    with server.accept()[0] as connection:
                        self.assertEqual(ADAPTER.receive_message(connection)[0], ADAPTER.HELLO)
                        ADAPTER.send_message(connection, ADAPTER.HELLO_ACK, bytes([ADAPTER.PROTOCOL_VERSION]))
                        self.assertTrue(source_started.wait(2))
                    runner.join(2)
                    stopped_without_cleanup = not runner.is_alive() and child.poll() is not None
                finally:
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=3)
                    runner.join(3)
                    child.stdout.close()
        self.assertTrue(stopped_without_cleanup, 'A frozen capture survived the media connection closing')
        self.assertEqual(failures, [])


class CameraSelectionTests(unittest.TestCase):
    def config(self, mode='auto'):
        return argparse.Namespace(mode=mode, device='/dev/video0', width=1920, height=1080,
            fps=30, bitrate=2_000_000, ffmpeg='ffmpeg', reconnect_delay=0)

    def test_default_is_auto(self):
        with patch.object(sys, 'argv', ['stream_webcam.py']):
            self.assertEqual(ADAPTER.arguments().mode, 'auto')

    def test_uvc_probe_sets_same_portable_capture_contract_and_preserves_other_fields(self):
        import struct
        initial = bytes(range(46))
        probe = ADAPTER.h264_probe(self.config(), initial)
        self.assertEqual(struct.unpack_from('<IIH', probe), (333333, 2000000, 0xA80F))
        self.assertEqual(struct.unpack_from('<HH', probe, 12), (1920, 1080))
        self.assertEqual(struct.unpack_from('<HH', probe, 20), (0x4240, 1000))
        self.assertEqual(probe[28:30], b'\x01\x01')
        self.assertEqual((probe[34], probe[35], probe[37]), (0, 0, 0))
        self.assertEqual(probe[38:], initial[38:])
        self.assertEqual(initial, bytes(range(46)))
        with self.assertRaisesRegex(RuntimeError, 'length'):
            ADAPTER.h264_probe(self.config(), bytes(45))

    def test_windows_unique_device_is_preserved_for_hardware_and_fallback(self):
        config = self.config()
        config.device = r'@device_pnp_\\?\usb#vid_046d&pid_0821#instance#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global'
        with patch.object(sys, 'platform', 'win32'), patch.object(ADAPTER, 'gstreamer_executable', return_value='gst-launch'):
            hardware = ADAPTER.gstreamer_command(config)
            software = ADAPTER.ffmpeg_command(config)
        path = config.device.removeprefix('@device_pnp_').replace(
            '{65e8773d-8f56-11d0-a3b9-00a0c9223196}', '{6994ad05-93ef-11d0-a3cc-00a0c9223196}')
        self.assertIn('device-path="' + path.replace('\\', '\\\\') + '"', hardware)
        self.assertNotIn('device-name=', ' '.join(hardware))
        self.assertIn('video=' + config.device, software)
        self.assertNotIn('libx264', hardware)

    def test_windows_friendly_name_remains_supported(self):
        config = self.config()
        config.device = 'HD Pro Webcam C920'
        with patch.object(sys, 'platform', 'win32'), patch.object(ADAPTER, 'gstreamer_executable', return_value='gst-launch'):
            command = ADAPTER.gstreamer_command(config)
        self.assertIn('device-name="HD Pro Webcam C920"', command)

    def test_linux_passthrough_copies_h264_without_encoder_or_filter(self):
        with patch.object(sys, 'platform', 'linux'):
            command = ADAPTER.passthrough_command(self.config())
        self.assertEqual(command[command.index('-input_format') + 1], 'h264')
        self.assertEqual(command[command.index('-c:v') + 1], 'copy')
        self.assertNotIn('libx264', command)
        self.assertNotIn('-vf', command)
        self.assertIn('h264_metadata=aud=insert', command)

    def test_linux_software_prefers_mjpeg_and_does_not_force_level_31(self):
        with patch.object(sys, 'platform', 'linux'), patch.object(ADAPTER.subprocess, 'run',
                return_value=subprocess.CompletedProcess([], 1, stderr=b'Compressed: mjpeg')):
            command = ADAPTER.ffmpeg_command(self.config('software'))
        self.assertEqual(command[command.index('-input_format') + 1], 'mjpeg')
        self.assertIn('libx264', command)
        self.assertNotIn('-level:v', command)

    def test_software_works_with_raw_only_camera(self):
        with patch.object(sys, 'platform', 'linux'), patch.object(ADAPTER.subprocess, 'run',
                return_value=subprocess.CompletedProcess([], 1, stderr=b'Raw: yuyv422')):
            command = ADAPTER.ffmpeg_command(self.config('software'))
        self.assertNotIn('-input_format', command)

    def test_auto_falls_back_once_and_preserves_requested_capture_settings(self):
        config = self.config()
        modes = []
        def stream(active):
            modes.append(active.mode)
            self.assertEqual((active.width, active.height, active.fps), (1920, 1080, 30))
            if len(modes) == 1:
                raise ADAPTER.PassthroughUnavailable('camera has no H.264')
            if len(modes) == 2:
                raise ConnectionError('media session closed')
            raise KeyboardInterrupt
        with patch.object(ADAPTER, 'arguments', return_value=config), patch.object(ADAPTER, 'stream_once', side_effect=stream):
            ADAPTER.main()
        self.assertEqual(modes, ['auto', 'software', 'software'])

    def test_media_connection_failure_does_not_trigger_software_fallback(self):
        config = self.config()
        modes = []
        def stream(active):
            modes.append(active.mode)
            if len(modes) == 1:
                raise ConnectionError('no authorized media session')
            raise KeyboardInterrupt
        with patch.object(ADAPTER, 'arguments', return_value=config), patch.object(ADAPTER, 'stream_once', side_effect=stream):
            ADAPTER.main()
        self.assertEqual(modes, ['auto', 'auto'])


class WindowsControlTests(unittest.TestCase):
    def driver(self, has_extension=True, refuse_dimensions=False):
        import ctypes
        import struct
        from ctypes import wintypes
        kernel = MagicMock()
        kernel.CreateFileW.return_value = 42
        config = CameraSelectionTests().config()
        config.device = r'@device_pnp_\\?\usb#instance#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global'
        probe = bytes(46)
        writes = []
        def ioctl(_handle, code, request, size, buffer, length, returned, _overlapped):
            nonlocal probe
            self.assertEqual(code, 0x2F0003)
            header = ctypes.string_at(request, size)
            selector, flags = struct.unpack_from('<II', header, 16)
            if size == 24:
                self.assertEqual(selector, 1)
                data = struct.pack('<II', 40, 2) + bytes(32)
            else:
                self.assertEqual(size, 32)
                node = struct.unpack_from('<I', header, 24)[0]
                if node != 1 or not has_extension:
                    error = OSError('property unavailable'); error.winerror = 1170
                    raise error
                if flags & 2:
                    writes.append((node, selector, ctypes.string_at(buffer, length)))
                    if selector == 1:
                        probe = ctypes.string_at(buffer, length)
                    data = b''
                else:
                    data = probe
                    if refuse_dimensions and writes:
                        data = bytearray(data); struct.pack_into('<HH', data, 12, 640, 480)
                        data = bytes(data)
            if data:
                ctypes.memmove(buffer, data, len(data))
            ctypes.cast(returned, ctypes.POINTER(wintypes.DWORD)).contents.value = len(data)
            return True
        kernel.DeviceIoControl.side_effect = ioctl
        return config, kernel, writes

    def test_discovers_uvc_node_configures_then_commits_and_releases_handle(self):
        config, kernel, writes = self.driver()
        with patch.object(ADAPTER.ctypes, 'WinDLL', return_value=kernel, create=True):
            ADAPTER.configure_windows_h264(config)
        self.assertEqual([(n, s) for n, s, _ in writes], [(1, 1), (1, 2)])
        self.assertEqual(writes[0][2], ADAPTER.h264_probe(config, bytes(46)))
        self.assertEqual(writes[1][2], writes[0][2])
        kernel.CloseHandle.assert_called_once_with(42)

    def test_ordinary_camera_without_h264_extension_is_not_modified(self):
        config, kernel, writes = self.driver(has_extension=False)
        with patch.object(ADAPTER.ctypes, 'WinDLL', return_value=kernel, create=True):
            ADAPTER.configure_windows_h264(config)
        self.assertEqual(writes, [])
        kernel.CloseHandle.assert_called_once_with(42)

    def test_rejected_dimensions_never_commit_and_release_handle(self):
        config, kernel, writes = self.driver(refuse_dimensions=True)
        with patch.object(ADAPTER.ctypes, 'WinDLL', return_value=kernel, create=True):
            with self.assertRaisesRegex(RuntimeError, 'dimensions'):
                ADAPTER.configure_windows_h264(config)
        self.assertEqual([(n, s) for n, s, _ in writes], [(1, 1)])
        kernel.CloseHandle.assert_called_once_with(42)


class HardwareValidationTests(unittest.TestCase):
    def rejected_source(self, data, message):
        child = subprocess.Popen([sys.executable, '-c',
            f'import sys,time; sys.stdout.buffer.write({data!r}); sys.stdout.buffer.flush(); time.sleep(30)'], stdout=subprocess.PIPE)
        failures = []
        with socket.socket() as server:
            server.bind(('127.0.0.1', 0))
            server.listen(1)
            server.settimeout(3)
            config = argparse.Namespace(mode='auto', device='fixture', width=640, height=480,
                fps=30, bitrate=2_000_000, stats_interval=0, host='127.0.0.1', port=server.getsockname()[1])
            def run():
                try:
                    ADAPTER.stream_once(config)
                except Exception as error:
                    failures.append(error)
            with patch.object(ADAPTER, 'source_process', return_value=(child, ADAPTER.ffmpeg_access_units(child.stdout))):
                runner = threading.Thread(target=run, daemon=True)
                runner.start()
                try:
                    with server.accept()[0] as connection:
                        connection.settimeout(3)
                        self.assertEqual(ADAPTER.receive_message(connection)[0], ADAPTER.HELLO)
                        ADAPTER.send_message(connection, ADAPTER.HELLO_ACK, bytes([1]))
                        # Invalid camera data must never be forwarded, and closing
                        # the producer must release the control reader too.
                        self.assertEqual(connection.recv(1), b'')
                    runner.join(2)
                    stopped = not runner.is_alive() and child.poll() is not None
                finally:
                    if child.poll() is None:
                        child.kill()
                    child.wait(timeout=3)
                    runner.join(3)
                    child.stdout.close()
        self.assertTrue(stopped)
        self.assertEqual(len(failures), 1)
        self.assertIsInstance(failures[0], ADAPTER.PassthroughUnavailable)
        self.assertIn(message, str(failures[0]))

    def test_invalid_hardware_stream_releases_capture_and_requests_fallback(self):
        cases = [
            (baseline_keyframe().replace(b'\x67\x42\xc0', b'\x67\x64\x00'), 'constrained-baseline'),
            (baseline_keyframe(1920, 1080), 'dimensions'),
            (b'\x00\x00\x01\x09\xf0\x00\x00\x01\x65\xaa', 'SPS and PPS'),
        ]
        for au, message in cases:
            with self.subTest(message=message):
                self.rejected_source(au * 2, message)

    def test_frozen_hardware_source_requests_fallback_and_closes_socket(self):
        original = ADAPTER.start_source_watchdog
        def expire(process, stopped, deadline, timed_out):
            deadline[0] = time.monotonic() - 1
            original(process, stopped, deadline, timed_out)
        with patch.object(ADAPTER, 'start_source_watchdog', side_effect=expire):
            self.rejected_source(b'', 'stalled')




class BackgroundCaptureTests(unittest.TestCase):
    def test_waiting_for_viewer_retries_without_log_spam_or_capture(self):
        config = argparse.Namespace(host="127.0.0.1", port=7002, reconnect_delay=1)
        output = io.StringIO()
        with patch.object(ADAPTER, "arguments", return_value=config), \
             patch.object(ADAPTER.signal, "signal"), \
             patch.object(ADAPTER.socket, "create_connection", side_effect=ConnectionRefusedError(111, "Connection refused")) as connect, \
             patch.object(ADAPTER.time, "sleep", side_effect=[None, None, KeyboardInterrupt]), \
             patch.object(ADAPTER, "source_process") as capture, \
             patch("sys.stdout", output):
            ADAPTER.main()
        self.assertEqual(connect.call_count, 3)
        capture.assert_not_called()
        self.assertEqual(output.getvalue(), "")

    def test_actual_camera_failure_remains_visible(self):
        output = io.StringIO()
        with patch.object(ADAPTER, "arguments", return_value=argparse.Namespace(reconnect_delay=1)), \
             patch.object(ADAPTER.signal, "signal"), \
             patch.object(ADAPTER, "stream_once", side_effect=RuntimeError("encoder failed")), \
             patch.object(ADAPTER.time, "sleep", side_effect=KeyboardInterrupt), \
             patch("sys.stdout", output):
            ADAPTER.main()
        self.assertIn("Camera adapter stopped: encoder failed", output.getvalue())

    def test_encoder_cannot_read_the_background_terminal(self):
        config = argparse.Namespace(mode="software", device="/dev/video0")
        with patch.object(ADAPTER, "configure_linux_framerate"), \
             patch.object(ADAPTER, "ffmpeg_command", return_value=["ffmpeg"]), \
             patch.object(ADAPTER.subprocess, "Popen") as start:
            ADAPTER.source_process(config)
        self.assertEqual(start.call_args.kwargs["stdin"], subprocess.DEVNULL)


if __name__ == "__main__":
    unittest.main()
