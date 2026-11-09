# Camera input

The Agent starts `fleetly-media` for an authorized publication. The separate
Python helper `media/example-camera-adapter/stream_webcam.py` waits for Media's
HELLO_ACK on loopback TCP 7002 before opening capture and stops capture when the
producer connection closes. It never enrolls or chooses a robot identity.

```sh
fleetlyhub-camera --device /dev/video0 --width 640 --height 480 --fps 15
```

That command is installed by the Linux package. For a source checkout run
`python media/example-camera-adapter/stream_webcam.py` with the same options.
The user needs video-device permissions and FFmpeg on PATH. Auto mode first
validates actual camera H.264; unsupported/unusable streams fall back to software
H.264. Exact devices/formats must be selected; camera model names do not guarantee
native H.264 support. Choose dimensions/frame rate your device actually supports.

Windows can list cameras using `ffmpeg -hide_banner -list_devices true -f dshow -i dummy`.
Pass the exact DirectShow device/alternative name. Ordinary cameras can use
`--mode software`; native H.264 uses the helper's GStreamer/UVC path and needs
those runtime tools. No GStreamer/FFmpeg executables are bundled in this source.

ROS/OEM sources can implement the same [Media interface](../contracts/media/v1/contract.md)
instead of using the USB helper. Feed complete encoded H.264 Annex-B access units,
not raw ROS images, MP4 bytes or arbitrary TCP data. Encode upstream when needed.
Only one producer may feed a publication. Source selection/encoding stays outside
the Agent. Media forwards encoded video without decoding/re-encoding it.

Reconnect with bounded backoff when no authorized publication is listening; do
not fill logs with repeated connection-refused messages. Preserve publication
identity when handling start/stop, and release device/child processes on stop.
Use the optional [camera systemd unit](../examples/systemd-user/livefeed_camera.service)
only after selecting the correct device/account. Starting the Agent does not
start the camera helper automatically.
