package main

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"testing"
	"time"
)

type waitingExecutor struct {
	started chan struct{}
	release chan struct{}
}

func (e *waitingExecutor) Execute(ctx context.Context, u Unit, action string) error {
	select {
	case e.started <- struct{}{}:
	default:
	}
	select {
	case <-e.release:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}
func TestControlExecutionDoesNotStopStatusHeartbeat(t *testing.T) {
	config := Config{TimeoutMS: 1000, HeartbeatMS: 100, StateDirectory: t.TempDir(), Units: []Unit{{"navigation.service", "user"}}, Controls: map[string][]string{"navigation.service": {"restart"}}}
	executor := &waitingExecutor{make(chan struct{}, 1), make(chan struct{})}
	controller, err := newController(config, executor)
	if err != nil {
		t.Fatal(err)
	}
	defer controller.Close()
	client, server := net.Pipe()
	defer server.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	latest := &latest{}
	latest.put(Observation{[]Service{{"navigation.service", "active", "running", []string{"restart"}}}, time.Now()})
	done := make(chan error, 1)
	go func() { done <- session(ctx, client, config, latest, controller) }()
	reader := bufio.NewReaderSize(server, 65536)
	server.SetDeadline(time.Now().Add(3 * time.Second))
	registration, err := readEnvelope(reader)
	if err != nil {
		t.Fatal(err)
	}
	var routes struct {
		Publish   []string `json:"publish_routes"`
		Subscribe []string `json:"subscribe_routes"`
	}
	json.Unmarshal(registration.Payload, &routes)
	if len(routes.Publish) != 2 || len(routes.Subscribe) != 1 || routes.Subscribe[0] != "services/control" {
		t.Fatal(routes)
	}
	if err = writeEnvelope(server, "agent/registered", map[string]any{"schema": "fleetly.local-registration-result.v1", "client_id": "service-bridge", "accepted": true}, time.Second); err != nil {
		t.Fatal(err)
	}
	snapshot(t, reader)
	now := time.Now().UnixMilli()
	command := Control{"robot.service-control.v1", "control-test", now, now + 30000, "navigation.service", "restart"}
	if err = writeEnvelope(server, "services/control", command, time.Second); err != nil {
		t.Fatal(err)
	}
	select {
	case <-executor.started:
	case <-time.After(time.Second):
		t.Fatal("executor did not receive control")
	}
	snapshot(t, reader) // An in-flight systemd action does not block live service reports.
	close(executor.release)
	for {
		envelope, err := readEnvelope(reader)
		if err != nil {
			t.Fatal(err)
		}
		if envelope.Topic == "services/status" {
			continue
		}
		if envelope.Topic != "services/control-results/control-test" {
			t.Fatal(envelope)
		}
		var result ControlResult
		json.Unmarshal(envelope.Payload, &result)
		if result.Status != "succeeded" {
			t.Fatal(result)
		}
		break
	}
	cancel()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("session worker did not stop")
	}
}
