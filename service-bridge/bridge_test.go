package main

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestDBusAuthenticationCannotHang(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bus")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		buffer := make([]byte, 1024)
		for {
			if _, err = conn.Read(buffer); err != nil {
				return
			}
		}
	}()
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	bus := &busSource{path: path, timeout: time.Second}
	start := time.Now()
	if _, err = bus.connect(ctx); err == nil {
		t.Fatal("unresponsive bus accepted")
	}
	if time.Since(start) > 500*time.Millisecond {
		t.Fatal("authentication exceeded its deadline")
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("connection leaked")
	}
}

func TestAgentErrorClosesDomainSession(t *testing.T) {
	client, server := net.Pipe()
	defer server.Close()
	l := &latest{}
	l.put(Observation{[]Service{}, time.Now()})
	done := make(chan error, 1)
	go func() { done <- session(context.Background(), client, Config{TimeoutMS: 1000, HeartbeatMS: 1000}, l) }()
	reader := bufio.NewReaderSize(server, 65536)
	register(t, server, reader)
	snapshot(t, reader)
	if err := writeEnvelope(server, "agent/error", map[string]string{"code": "forbidden_route"}, time.Second); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if err == nil || !strings.Contains(err.Error(), "rejected service data") {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("session did not reconnect on Agent rejection")
	}
}

func TestConfigRejectsAmbiguousOrUnsafeInventory(t *testing.T) {
	for _, input := range []string{
		`{"services":[{"name":"../bad.service","bus":"user"}]}`,
		`{"services":[{"name":"a.service","bus":"user"},{"name":"a.service","bus":"system"}]}`,
		`{"services":[{"name":"a.service","bus":"session"}]}`,
		`{"services":[],"agent_address":"192.0.2.10:7001"}`,
		`{"services":[],"remote_control":true}`,
		`{"services":null}`, `{"services":[]} {}`, `{"services":[],"heartbeat_ms":30000}`,
	} {
		if _, err := loadConfig(strings.NewReader(input)); err == nil {
			t.Errorf("accepted %s", input)
		}
	}
}
func TestExplicitEmptyInventory(t *testing.T) {
	c, err := loadConfig(strings.NewReader(`{"services":[]}`))
	if err != nil {
		t.Fatal(err)
	}
	if c.Units == nil || c.Address != "127.0.0.1:7001" {
		t.Fatal(c)
	}
}

type failedSource struct{}

func (failedSource) Read(ctx context.Context, u Unit) (Service, error) {
	<-ctx.Done()
	return Service{}, ctx.Err()
}
func TestBoundedBusFailuresPreserveCompleteInventory(t *testing.T) {
	c := Config{TimeoutMS: 100, Units: []Unit{{"a.service", "user"}, {"b.service", "system"}}}
	began := time.Now()
	o := observe(context.Background(), failedSource{}, c)
	if time.Since(began) > 500*time.Millisecond {
		t.Fatal("queries were not concurrent/bounded")
	}
	if len(o.Services) != 2 {
		t.Fatal(o)
	}
	for _, s := range o.Services {
		if s.Status != "unknown" || s.Substatus != "unavailable" || s.Actions == nil || len(s.Actions) != 0 {
			t.Fatal(s)
		}
	}
}
func snapshot(t *testing.T, reader *bufio.Reader) Snapshot {
	t.Helper()
	e, err := readEnvelope(reader)
	if err != nil {
		t.Fatal(err)
	}
	if e.Topic != "services/status" {
		t.Fatal(e)
	}
	var s Snapshot
	if err = json.Unmarshal(e.Payload, &s); err != nil {
		t.Fatal(err)
	}
	return s
}
func register(t *testing.T, conn net.Conn, reader *bufio.Reader) {
	t.Helper()
	conn.SetDeadline(time.Now().Add(2 * time.Second))
	e, err := readEnvelope(reader)
	if err != nil {
		t.Fatal(err)
	}
	var r struct {
		ID        string   `json:"client_id"`
		Publish   []string `json:"publish_routes"`
		Subscribe []string `json:"subscribe_routes"`
	}
	json.Unmarshal(e.Payload, &r)
	if e.Topic != "agent/register" || r.ID != "service-bridge" || len(r.Publish) != 1 || r.Publish[0] != "services/status" || len(r.Subscribe) != 0 {
		t.Fatal(string(e.Payload))
	}
	if err = writeEnvelope(conn, "agent/registered", map[string]any{"schema": "fleetly.local-registration-result.v1", "client_id": "service-bridge", "accepted": true}, time.Second); err != nil {
		t.Fatal(err)
	}
}
func TestSessionRegistrationChangesHeartbeatAndReconnect(t *testing.T) {
	c := Config{TimeoutMS: 1000, HeartbeatMS: 200}
	l := &latest{}
	l.put(Observation{[]Service{{"a.service", "inactive", "dead", []string{}}}, time.Now()})
	for attempt := 0; attempt < 2; attempt++ {
		client, server := net.Pipe()
		ctx, cancel := context.WithCancel(context.Background())
		done := make(chan error, 1)
		go func() { done <- session(ctx, client, c, l) }()
		reader := bufio.NewReaderSize(server, 65536)
		register(t, server, reader)
		first := snapshot(t, reader)
		want := "inactive"
		if attempt == 1 {
			want = "active"
		}
		if first.Services[0].Status != want {
			t.Fatal(first)
		}
		if attempt == 0 {
			l.put(Observation{[]Service{{"a.service", "active", "running", []string{}}}, time.Now()})
			next := snapshot(t, reader)
			if next.Services[0].Status != "active" {
				t.Fatal(next)
			}
			heartbeat := snapshot(t, reader)
			if len(heartbeat.Services) != 1 {
				t.Fatal(heartbeat)
			}
		}
		cancel()
		server.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Fatal("shutdown hung")
		}
	}
}
func TestSessionRejectsRegistrationFailure(t *testing.T) {
	client, server := net.Pipe()
	defer server.Close()
	l := &latest{}
	done := make(chan error, 1)
	go func() { done <- session(context.Background(), client, Config{TimeoutMS: 100}, l) }()
	reader := bufio.NewReaderSize(server, 65536)
	if _, err := readEnvelope(reader); err != nil {
		t.Fatal(err)
	}
	writeEnvelope(server, "agent/error", map[string]string{"code": "duplicate_client"}, time.Second)
	if err := <-done; err == nil {
		t.Fatal("registration rejection accepted")
	}
}
func TestSessionStopsOnStaleObservation(t *testing.T) {
	client, server := net.Pipe()
	defer server.Close()
	l := &latest{}
	l.put(Observation{[]Service{}, time.Now().Add(-20 * time.Second)})
	done := make(chan error, 1)
	go func() { done <- session(context.Background(), client, Config{TimeoutMS: 1000}, l) }()
	register(t, server, bufio.NewReaderSize(server, 65536))
	if err := <-done; err == nil || !strings.Contains(err.Error(), "stale") {
		t.Fatal(err)
	}
}
func TestAgentPartialAndOversizedMessages(t *testing.T) {
	for _, s := range []string{`{"protocol":"fleetly.local.v1"}`, strings.Repeat("x", 65537) + "\n"} {
		if _, err := readEnvelope(bufio.NewReaderSize(strings.NewReader(s), 65536)); err == nil {
			t.Fatal("bad framing accepted")
		}
	}
}
func TestUnresponsiveRegistrationHasDeadline(t *testing.T) {
	client, server := net.Pipe()
	defer server.Close()
	done := make(chan error, 1)
	go func() { done <- session(context.Background(), client, Config{TimeoutMS: 100}, &latest{}) }()
	if _, err := readEnvelope(bufio.NewReaderSize(server, 65536)); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("missing timeout")
		}
	case <-time.After(time.Second):
		t.Fatal("unbounded registration")
	}
}

func TestAgentAndBridgeCannotBeMadeControllable(t *testing.T) {
	for _, name := range []string{"fleetlyhub-agent.service", "fleetly-agent.service", "service-bridge.service", "bigbot_service_bridge.service"} {
		input := `{"services":[{"name":"` + name + `","bus":"user"}],"controls":{"` + name + `":["start","stop","restart"]}}`
		if _, err := loadConfig(strings.NewReader(input)); err == nil {
			t.Errorf("accepted control permissions for %s", name)
		}
	}
}
