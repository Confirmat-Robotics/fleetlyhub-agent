package main

import (
	"bytes"
	"encoding/binary"
	"testing"
)

func framed(messageType byte, payload []byte) []byte {
	body := append([]byte{messageType}, payload...)
	result := make([]byte, 4, 4+len(body))
	binary.BigEndian.PutUint32(result, uint32(len(body)))
	return append(result, body...)
}

func validHelloPayload() []byte {
	payload := []byte{protocolVersion, 1}
	for _, value := range []uint16{1280, 720, 30} {
		var data [2]byte
		binary.BigEndian.PutUint16(data[:], value)
		payload = append(payload, data[:]...)
	}
	var bitrate [4]byte
	binary.BigEndian.PutUint32(bitrate[:], 2_000_000)
	payload = append(payload, bitrate[:]...)
	return append(payload, capKeyframeRequest|capRateControl)
}

func TestReadValidHello(t *testing.T) {
	message, err := readClientMessage(bytes.NewReader(framed(clientHello, validHelloPayload())))
	if err != nil {
		t.Fatal(err)
	}
	if message.Hello == nil || message.Hello.Width != 1280 || !message.Hello.supportsRateControl() {
		t.Fatalf("unexpected HELLO: %+v", message.Hello)
	}
}

func TestRejectOversizedMessage(t *testing.T) {
	header := make([]byte, 4)
	binary.BigEndian.PutUint32(header, maxMessageBytes+1)
	if _, err := readClientMessage(bytes.NewReader(header)); err == nil {
		t.Fatal("oversized message was accepted")
	}
}

func TestKeyframeRequiresSPSAndPPS(t *testing.T) {
	payload := make([]byte, 13)
	payload[0] = 1
	binary.BigEndian.PutUint16(payload[1:3], 1280)
	binary.BigEndian.PutUint16(payload[3:5], 720)
	binary.BigEndian.PutUint64(payload[5:13], 1234)
	payload = append(payload, 0, 0, 0, 1, 0x65, 1)
	if _, err := readClientMessage(bytes.NewReader(framed(clientVideoFrame, payload))); err == nil {
		t.Fatal("IDR without SPS/PPS was accepted")
	}

	payload = payload[:13]
	payload = append(payload,
		0, 0, 0, 1, 0x67, 1,
		0, 0, 0, 1, 0x68, 1,
		0, 0, 0, 1, 0x65, 1,
	)
	if _, err := readClientMessage(bytes.NewReader(framed(clientVideoFrame, payload))); err != nil {
		t.Fatalf("valid keyframe rejected: %v", err)
	}
}

func TestWriteRateControl(t *testing.T) {
	var output bytes.Buffer
	err := writeServerMessage(&output, serverMessage{Type: serverRateControl, BitrateBPS: 2_000_000, FPS: 30})
	if err != nil {
		t.Fatal(err)
	}
	data := output.Bytes()
	if binary.BigEndian.Uint32(data[:4]) != 7 || data[4] != serverRateControl || binary.BigEndian.Uint32(data[5:9]) != 2_000_000 {
		t.Fatalf("unexpected RATE_CONTROL bytes: %v", data)
	}
}
