# Connect an existing robot camera

Read [the shared instructions](README.md), [camera operation](../camera.md) and
the [binary Media contract](../../contracts/media/v1/contract.md). Video is
separate from telemetry/diagnostics and robot task controls. Choose one producer
for a publication; do not replace a working robot camera driver unnecessarily.

## Inspect and select the source

Determine OS, device/topic, source encoding, dimensions, frame rate, available
encoder and whether another process owns the camera. Preserve the existing
vision pipeline. Agree whether to use the bundled USB helper, tap an existing
encoded stream, or encode frames from an existing ROS/OEM source. Model names
alone do not establish hardware H.264 support.

### Existing USB helper

On Linux the APT package provides:

```sh
fleetlyhub-camera --device /dev/video0 --width 640 --height 480 --fps 15
```

For a source build, use the same options with:

```sh
python media/example-camera-adapter/stream_webcam.py --device /dev/video0 --width 640 --height 480 --fps 15
```

Select actual supported formats and the account with camera permissions. On
Windows use the exact DirectShow device/alternative name; see camera operation
for device discovery and FFmpeg/GStreamer requirements. Do not use the Linux
device path or assume an encoder is bundled on Windows.

The helper waits quietly for an authorized Media listener, completes HELLO/ACK
and then starts capture. It releases capture when the socket closes. Run it as
its own process/service; Agent startup alone does not launch it. The optional
[systemd unit](../../examples/systemd-user/livefeed_camera.service) needs the
actual device/account. A waiting helper can remain running while no one watches;
this must not keep camera capture on.

### Existing ROS/OEM video producer

For ROS `sensor_msgs/Image` or compressed image topics inspect the real encoding
and QoS. Raw images and JPEG/PNG are not H.264. A ROS compressed-message wrapper
is not automatically a complete Annex-B access unit either. Subscribe to the
existing pipeline, encode upstream if needed, and feed Media through the binary
interface. Do not send ROS images, MP4 container bytes or naked H.264 bytes to TCP
7002. Prefer the existing helper if no custom video producer is needed.

Generate a source adapter using the [Media implementation/helper](../../media/example-camera-adapter/stream_webcam.py)
and binary contract as references. Supply constrained-baseline, low-latency H.264
with complete access units, SPS/PPS on IDRs, no B-frames and regular IDRs. Reuse
encoded video when compatible; avoid decode/re-encode just for forwarding.
Pace recorded sources instead of sending the whole file immediately; after a
stall recover at a new IDR rather than bursting stale frames.

## Follow publication lifetime

The Agent starts Media with its private short-lived grant. A producer must not
start Media itself, construct cloud credentials or choose a cloud room. A custom
robot adapter can receive [media/start](../../contracts/local/v1/examples/media-start.json)
and [media/stop](../../contracts/local/v1/examples/media-stop.json) after registration.
Use the supplied loopback endpoint, publication ID and expiry. A waiting external
helper can instead react to the Media listener's availability.

Connect, send binary HELLO, wait for HELLO_ACK and only then open/read capture.
Start each connection at a valid IDR. Handle keyframe/rate-control messages for
capabilities actually advertised; fixed-rate producers must not claim capabilities
they cannot honor. Frame timestamps are monotonic microseconds, not UTC JSON
milliseconds. Implement complete reads, bounded lengths and bounded queues.

Match stop to the current publication; ignore an old stop for a replacement.
On socket EOF, stop/expiry or process shutdown release the source and all encoder
children, including if the source has stalled. Retry absent listeners with bounded
backoff without connection-refused log floods. MQTT reconnect and robot task
PAUSE/STOP must not independently end a valid video session. Normal grant renewal
keeps the same producer connection; do not restart capture every few minutes.

If implementing `camera/status`, use its [schema](../../contracts/local/v1/schemas/robot.camera-status.v1.schema.json)
and real evidence. A running helper, connected Agent or `media/start` request
alone is not proof of published video. Avoid duplicate/conflicting status owners;
the Agent/Hub also observe publication lifetime. Unavailable/idle/publishing/error
are the defined values; error needs an error code and other states omit it.

## Acceptance checks

- No viewer: helper can wait, port 7002 may be closed, capture is released and
  logging stays quiet. Repeated startup must not create duplicate producers.
- One TEST-app viewer: verify actual changing video, codec/dimensions/frame rate,
  and source/encoder cleanup after the final publication closes.
- Two viewers: one leaving must not interrupt the other. Status/Services/Camera
  switches within robot details must not restart capture. A temporary navigation
  grace can retain a publication; do not impose a competing source-side timer.
- Test a stalled/disconnected source, a replacement publication followed by an
  old stop, and an Agent/Media restart. Preserve supported manual Retry behavior.
- Run the [source checks](../build.md). Unit/framing tests do not establish actual
  camera or Android playback compatibility; record those observations separately.
