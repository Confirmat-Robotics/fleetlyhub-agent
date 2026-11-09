package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"time"
)

const protocol = "fleetly.local.v1"

type Envelope struct {
	Protocol string          `json:"protocol"`
	Topic    string          `json:"topic"`
	Payload  json.RawMessage `json:"payload"`
}
type Snapshot struct {
	Schema   string    `json:"schema"`
	TS       int64     `json:"ts"`
	Services []Service `json:"services"`
}

func writeEnvelope(conn net.Conn, topic string, payload any, timeout time.Duration) error {
	data, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	line, err := json.Marshal(Envelope{protocol, topic, data})
	if err != nil {
		return err
	}
	if len(line) >= 65536 {
		return fmt.Errorf("local line too large")
	}
	// Leave room for Agent transport metadata within the MQTT 16 KiB limit.
	if topic == "services/status" && len(line) > 15000 {
		return fmt.Errorf("services snapshot too large")
	}
	if err = conn.SetWriteDeadline(time.Now().Add(timeout)); err != nil {
		return err
	}
	line = append(line, '\n')
	for len(line) > 0 {
		n, e := conn.Write(line)
		if e != nil {
			return e
		}
		if n == 0 {
			return io.ErrShortWrite
		}
		line = line[n:]
	}
	return nil
}
func readEnvelope(reader *bufio.Reader) (Envelope, error) {
	line, err := reader.ReadSlice('\n')
	if err != nil {
		return Envelope{}, err
	}
	if len(line) > 65536 {
		return Envelope{}, fmt.Errorf("oversized Agent line")
	}
	var e Envelope
	if err = json.Unmarshal(line, &e); err != nil {
		return e, err
	}
	if e.Protocol != protocol || e.Topic == "" || len(e.Payload) == 0 {
		return e, fmt.Errorf("invalid Agent envelope")
	}
	return e, nil
}
func session(ctx context.Context, conn net.Conn, c Config, l *latest, controllers ...*controller) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	defer conn.Close()
	// Closing interrupts bounded reads/writes promptly on SIGTERM or SIGINT.
	done := make(chan struct{})
	defer close(done)
	go func() {
		select {
		case <-ctx.Done():
			conn.Close()
		case <-done:
		}
	}()
	registration := map[string]any{"schema": "fleetly.local-registration.v1", "client_id": "service-bridge",
		"publish_routes": []string{"services/status"}, "subscribe_routes": []string{}}
	var controls *controller
	if len(controllers) > 0 {
		controls = controllers[0]
	}
	if controls != nil {
		registration["publish_routes"] = []string{"services/status", "services/control-results/+"}
		registration["subscribe_routes"] = []string{"services/control"}
	}
	if err := writeEnvelope(conn, "agent/register", registration, milliseconds(c.TimeoutMS)); err != nil {
		return err
	}
	if err := conn.SetReadDeadline(time.Now().Add(milliseconds(c.TimeoutMS))); err != nil {
		return err
	}
	reader := bufio.NewReaderSize(conn, 65536)
	reply, err := readEnvelope(reader)
	if err != nil {
		return err
	}
	var accepted struct {
		Schema   string `json:"schema"`
		ID       string `json:"client_id"`
		Accepted bool   `json:"accepted"`
	}
	if err = json.Unmarshal(reply.Payload, &accepted); err != nil {
		return err
	}
	if reply.Topic != "agent/registered" || accepted.Schema != "fleetly.local-registration-result.v1" || accepted.ID != "service-bridge" || !accepted.Accepted {
		return fmt.Errorf("service-bridge registration rejected")
	}
	if err = conn.SetReadDeadline(time.Time{}); err != nil {
		return err
	}
	incoming := make(chan error, 1)
	commands := make(chan Control, 8)
	type finishedControl struct {
		id     string
		result ControlResult
	}
	results := make(chan finishedControl, 8)
	workerDone := make(chan struct{})
	go func() {
		defer close(workerDone)
		for {
			select {
			case <-ctx.Done():
				return
			case command := <-commands:
				if controls == nil {
					continue
				}
				result := controls.handle(ctx, command)
				select {
				case results <- finishedControl{command.ID, result}:
				case <-ctx.Done():
					return
				}
			}
		}
	}()
	defer func() { cancel(); <-workerDone }()
	readerDone := make(chan struct{})
	defer func() { conn.Close(); <-readerDone }()
	go func() {
		defer close(readerDone)
		for {
			message, e := readEnvelope(reader)
			if e != nil {
				incoming <- e
				return
			}
			if message.Topic == "agent/error" {
				incoming <- fmt.Errorf("Agent rejected service data: %s", message.Payload)
				return
			}
			if message.Topic == "services/control" && controls != nil {
				command, e := parseControl(message.Payload)
				if e != nil {
					incoming <- fmt.Errorf("invalid control payload: %w", e)
					return
				}
				select {
				case commands <- command:
				case <-ctx.Done():
					return
				default:
					incoming <- fmt.Errorf("control queue full")
					return
				}
				continue
			}
			incoming <- fmt.Errorf("unexpected Agent topic: %s", message.Topic)
			return
		}
	}()
	log.Print("service-bridge registered; reporting configured units")
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	previous := ""
	lastSend := time.Time{}
	for {
		observation, ready := l.get()
		if ready {
			if time.Since(observation.At) > 10*time.Second {
				return fmt.Errorf("systemd observation stale")
			}
			signature, _ := json.Marshal(observation.Services)
			if string(signature) != previous || time.Since(lastSend) >= milliseconds(c.HeartbeatMS) {
				snapshot := Snapshot{"robot.services-status.v1", observation.At.UnixMilli(), observation.Services}
				if err = writeEnvelope(conn, "services/status", snapshot, milliseconds(c.TimeoutMS)); err != nil {
					return err
				}
				if string(signature) != previous {
					log.Printf("services changed: %s", signature)
				}
				previous = string(signature)
				lastSend = time.Now()
			}
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case err = <-incoming:
			return err
		case completed := <-results:
			if err = writeEnvelope(conn, "services/control-results/"+completed.id, completed.result, milliseconds(c.TimeoutMS)); err != nil {
				return err
			}
		case <-ticker.C:
		}
	}
}
func run(ctx context.Context, c Config, l *latest, controllers ...*controller) {
	delay := time.Second
	for ctx.Err() == nil {
		conn, err := (&net.Dialer{Timeout: milliseconds(c.TimeoutMS)}).DialContext(ctx, "tcp", c.Address)
		began := time.Now()
		if err == nil {
			err = session(ctx, conn, c, l, controllers...)
		}
		if ctx.Err() != nil {
			return
		}
		log.Printf("Agent connection ended: %v; retry in %s", err, delay)
		if time.Since(began) > 10*time.Second {
			delay = time.Second
		}
		timer := time.NewTimer(delay)
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
		if delay < 8*time.Second {
			delay *= 2
		}
	}
}
