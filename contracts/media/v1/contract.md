# Fleetly Local Media Protocol v1

The local media protocol carries complete pre-encoded H.264 access units from
one robot source adapter to `fleetly-media`. It is bound to loopback and does
not carry robot identity or LiveKit credentials.

## Transport

- TCP on `127.0.0.1:7002` by default.
- One producer connection at a time.
- Integer fields use network byte order (big-endian).
- Every message starts with a four-byte `message_length`, followed by a
  one-byte message type and its payload.
- `message_length` includes the type and payload but excludes the length field.
- Messages larger than 4 MiB are rejected before payload allocation.

## Producer Messages

### HELLO (`0x01`)

HELLO must be the first message and may be sent only once.

| Field | Type | Meaning |
| --- | --- | --- |
| `protocol_version` | `u8` | Must be `1` |
| `codec` | `u8` | `1` = H.264 |
| `width` | `u16` | Encoded width |
| `height` | `u16` | Encoded height |
| `fps` | `u16` | Nominal frame rate |
| `bitrate_bps` | `u32` | Nominal encoded bitrate |
| `capabilities` | `u8` | Control support flags |

Capability bit 0 means the producer can force a keyframe. Bit 1 means it can
apply live bitrate and frame-rate targets.

### VIDEO_FRAME (`0x02`)

| Field | Type | Meaning |
| --- | --- | --- |
| `flags` | `u8` | Bit 0 means keyframe |
| `width` | `u16` | Frame width |
| `height` | `u16` | Frame height |
| `timestamp_us` | `u64` | Monotonically increasing capture timestamp |
| `access_unit` | bytes | One complete H.264 Annex-B access unit |

The keyframe flag must agree with the NAL units. Every IDR access unit must
contain SPS and PPS. The initial frame must be an IDR; delta frames are dropped
until one is received.

The H.264 stream must use constrained-baseline profile, packetization mode 1,
no B-frames, no look-ahead, and approximately one IDR per second. Producers
should use zero-latency encoder settings.

## Fleetly Media Messages

### HELLO_ACK (`0x80`)

One-byte payload containing the accepted protocol version.

### KEYFRAME_REQUEST (`0x81`)

No payload. Sent after a LiveKit PLI/FIR or when local overload invalidates the
current delta-frame chain. Producers advertising capability bit 0 must force an
IDR containing SPS and PPS.

### RATE_CONTROL (`0x82`)

| Field | Type | Meaning |
| --- | --- | --- |
| `target_bitrate_bps` | `u32` | LiveKit target bitrate |
| `target_fps` | `u16` | LiveKit target frame rate |

Producers advertising capability bit 1 should apply the newest target. Others
may continue at their declared fixed rate.

## Overload Recovery

`fleetly-media` keeps at most a small bounded queue. When it cannot preserve a
valid low-latency H.264 dependency chain, it clears queued delta frames,
requests a new keyframe, discards all delta frames, and resumes at the next IDR.
It never intentionally replays seconds of stale video.
