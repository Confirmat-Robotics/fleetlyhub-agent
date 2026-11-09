package main

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

const (
	protocolVersion = 1
	maxMessageBytes = 4 * 1024 * 1024

	capKeyframeRequest = 1 << 0
	capRateControl     = 1 << 1

	clientHello      = 0x01
	clientVideoFrame = 0x02

	serverHelloAck        = 0x80
	serverKeyframeRequest = 0x81
	serverRateControl     = 0x82
)

type hello struct {
	Width        uint16
	Height       uint16
	FPS          uint16
	BitrateBPS   uint32
	Capabilities uint8
}

func (h hello) supportsKeyframeRequests() bool { return h.Capabilities&capKeyframeRequest != 0 }
func (h hello) supportsRateControl() bool      { return h.Capabilities&capRateControl != 0 }

type videoFrame struct {
	Keyframe   bool
	Width      uint16
	Height     uint16
	Timestamp  uint64
	AccessUnit []byte
}

type clientMessage struct {
	Hello *hello
	Frame *videoFrame
}

type serverMessage struct {
	Type       byte
	BitrateBPS uint32
	FPS        uint16
}

func readClientMessage(reader io.Reader) (clientMessage, error) {
	var header [4]byte
	if _, err := io.ReadFull(reader, header[:]); err != nil {
		return clientMessage{}, err
	}
	length := binary.BigEndian.Uint32(header[:])
	if length == 0 || length > maxMessageBytes {
		return clientMessage{}, fmt.Errorf("invalid media message length %d", length)
	}
	body := make([]byte, length)
	if _, err := io.ReadFull(reader, body); err != nil {
		return clientMessage{}, err
	}
	switch body[0] {
	case clientHello:
		h, err := parseHello(body[1:])
		return clientMessage{Hello: h}, err
	case clientVideoFrame:
		frame, err := parseVideoFrame(body[1:])
		return clientMessage{Frame: frame}, err
	default:
		return clientMessage{}, fmt.Errorf("unsupported client message type 0x%02x", body[0])
	}
}

func parseHello(payload []byte) (*hello, error) {
	if len(payload) != 13 {
		return nil, errors.New("HELLO payload must contain exactly 13 bytes")
	}
	if payload[0] != protocolVersion {
		return nil, fmt.Errorf("unsupported media protocol version %d", payload[0])
	}
	if payload[1] != 1 {
		return nil, fmt.Errorf("unsupported codec %d", payload[1])
	}
	h := &hello{
		Width:        binary.BigEndian.Uint16(payload[2:4]),
		Height:       binary.BigEndian.Uint16(payload[4:6]),
		FPS:          binary.BigEndian.Uint16(payload[6:8]),
		BitrateBPS:   binary.BigEndian.Uint32(payload[8:12]),
		Capabilities: payload[12],
	}
	if h.Width == 0 || h.Height == 0 || h.Width > 3840 || h.Height > 2160 || h.Width%2 != 0 || h.Height%2 != 0 {
		return nil, fmt.Errorf("invalid H.264 dimensions %dx%d", h.Width, h.Height)
	}
	if h.FPS < 1 || h.FPS > 60 {
		return nil, errors.New("fps must be between 1 and 60")
	}
	if h.BitrateBPS == 0 || h.BitrateBPS > 50_000_000 {
		return nil, errors.New("bitrate must be between 1 and 50000000 bits per second")
	}
	if h.Capabilities & ^uint8(capKeyframeRequest|capRateControl) != 0 {
		return nil, errors.New("HELLO contains unsupported capability flags")
	}
	return h, nil
}

func parseVideoFrame(payload []byte) (*videoFrame, error) {
	if len(payload) < 14 {
		return nil, errors.New("VIDEO_FRAME payload is too short")
	}
	if payload[0]&^byte(1) != 0 {
		return nil, errors.New("VIDEO_FRAME contains unsupported flags")
	}
	frame := &videoFrame{
		Keyframe:   payload[0]&1 != 0,
		Width:      binary.BigEndian.Uint16(payload[1:3]),
		Height:     binary.BigEndian.Uint16(payload[3:5]),
		Timestamp:  binary.BigEndian.Uint64(payload[5:13]),
		AccessUnit: append([]byte(nil), payload[13:]...),
	}
	if frame.Width == 0 || frame.Height == 0 {
		return nil, errors.New("VIDEO_FRAME dimensions must be nonzero")
	}
	types := annexBNALTypes(frame.AccessUnit)
	if len(types) == 0 {
		return nil, errors.New("VIDEO_FRAME must contain H.264 Annex-B NAL units")
	}
	containsIDR := types[5]
	if frame.Keyframe != containsIDR {
		return nil, errors.New("VIDEO_FRAME keyframe flag does not match its H.264 access unit")
	}
	if frame.Keyframe && (!types[7] || !types[8]) {
		return nil, errors.New("H.264 keyframes must contain SPS and PPS NAL units")
	}
	return frame, nil
}

func annexBNALTypes(data []byte) map[byte]bool {
	types := make(map[byte]bool)
	for i := 0; i+3 < len(data); {
		header := -1
		if i+4 <= len(data) && data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
			header = i + 4
		} else if data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
			header = i + 3
		}
		if header >= 0 {
			if header < len(data) {
				types[data[header]&0x1f] = true
			}
			i = header
		} else {
			i++
		}
	}
	return types
}

func writeServerMessage(writer io.Writer, message serverMessage) error {
	body := []byte{message.Type}
	switch message.Type {
	case serverHelloAck:
		body = append(body, protocolVersion)
	case serverKeyframeRequest:
	case serverRateControl:
		body = make([]byte, 7)
		body[0] = serverRateControl
		binary.BigEndian.PutUint32(body[1:5], message.BitrateBPS)
		binary.BigEndian.PutUint16(body[5:7], message.FPS)
	default:
		return fmt.Errorf("unsupported server message type 0x%02x", message.Type)
	}
	header := make([]byte, 4)
	binary.BigEndian.PutUint32(header, uint32(len(body)))
	if _, err := writer.Write(header); err != nil {
		return err
	}
	_, err := writer.Write(body)
	return err
}
