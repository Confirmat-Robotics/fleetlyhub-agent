package main

import (
	"context"
	"fmt"
	"github.com/godbus/dbus/v5"
	"net"
	"os"
	"sync"
	"time"
)

type Service struct {
	Name      string   `json:"service"`
	Status    string   `json:"status"`
	Substatus string   `json:"substatus"`
	Actions   []string `json:"available_actions"`
}
type Observation struct {
	Services []Service
	At       time.Time
}
type source interface {
	Read(context.Context, Unit) (Service, error)
}
type busSource struct {
	mu      sync.Mutex
	conn    *dbus.Conn
	path    string
	timeout time.Duration
}
type systemdSource struct{ buses map[string]*busSource }

func newSystemdSource(timeout time.Duration) *systemdSource {
	return &systemdSource{map[string]*busSource{
		"user":   {path: fmt.Sprintf("/run/user/%d/bus", os.Getuid()), timeout: timeout},
		"system": {path: "/run/dbus/system_bus_socket", timeout: timeout},
	}}
}
func (s *busSource) connect(ctx context.Context) (*dbus.Conn, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn != nil && s.conn.Connected() {
		return s.conn, nil
	}
	raw, err := (&net.Dialer{Timeout: s.timeout}).DialContext(ctx, "unix", s.path)
	if err != nil {
		return nil, err
	}
	deadline := time.Now().Add(s.timeout)
	if contextDeadline, ok := ctx.Deadline(); ok && contextDeadline.Before(deadline) {
		deadline = contextDeadline
	}
	if err = raw.SetDeadline(deadline); err != nil {
		raw.Close()
		return nil, err
	}
	conn, err := dbus.NewConn(raw)
	if err != nil {
		raw.Close()
		return nil, err
	}
	if err = conn.Auth(nil); err == nil {
		err = conn.Hello()
	}
	if err != nil {
		conn.Close()
		return nil, err
	}
	if err = raw.SetDeadline(time.Time{}); err != nil {
		conn.Close()
		return nil, err
	}
	s.conn = conn
	return conn, nil
}
func (s *busSource) invalidate(conn *dbus.Conn) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.conn == conn {
		conn.Close()
		s.conn = nil
	}
}
func (s *systemdSource) Close() {
	for _, b := range s.buses {
		b.mu.Lock()
		if b.conn != nil {
			b.conn.Close()
		}
		b.mu.Unlock()
	}
}
func (s *systemdSource) Read(ctx context.Context, u Unit) (Service, error) {
	result := Service{Name: u.Name, Status: "unknown", Substatus: "unavailable", Actions: []string{}}
	b := s.buses[u.Bus]
	conn, err := b.connect(ctx)
	if err != nil {
		return result, err
	}
	var path dbus.ObjectPath
	// Load metadata even for an inactive/unloaded unit. This does not start it.
	err = conn.Object("org.freedesktop.systemd1", "/org/freedesktop/systemd1").CallWithContext(ctx,
		"org.freedesktop.systemd1.Manager.LoadUnit", 0, u.Name).Store(&path)
	if err != nil {
		if ctx.Err() != nil || !conn.Connected() {
			b.invalidate(conn)
		}
		return result, err
	}
	var properties map[string]dbus.Variant
	err = conn.Object("org.freedesktop.systemd1", path).CallWithContext(ctx,
		"org.freedesktop.DBus.Properties.GetAll", 0, "org.freedesktop.systemd1.Unit").Store(&properties)
	if err != nil {
		if ctx.Err() != nil || !conn.Connected() {
			b.invalidate(conn)
		}
		return result, err
	}
	value := func(key string) string { v, _ := properties[key].Value().(string); return v }
	if value("LoadState") == "not-found" {
		result.Substatus = "not-found"
		return result, nil
	}
	result.Status = value("ActiveState")
	result.Substatus = value("SubState")
	if !stateName.MatchString(result.Status) || len(result.Status) > 32 || !stateName.MatchString(result.Substatus) {
		return Service{Name: u.Name, Status: "unknown", Substatus: "unavailable", Actions: []string{}}, fmt.Errorf("invalid systemd properties")
	}
	return result, nil
}

func observe(ctx context.Context, s source, c Config) Observation {
	statuses := make([]Service, len(c.Units))
	var wg sync.WaitGroup
	for index, u := range c.Units {
		wg.Add(1)
		go func(index int, u Unit) {
			defer wg.Done()
			deadline, cancel := context.WithTimeout(ctx, milliseconds(c.TimeoutMS))
			defer cancel()
			status, err := s.Read(deadline, u)
			if err != nil {
				status = Service{Name: u.Name, Status: "unknown", Substatus: "unavailable", Actions: []string{}}
			}
			if err == nil && status.Status != "unknown" && c.Controls[u.Name] != nil {
				status.Actions = append([]string{}, c.Controls[u.Name]...)
			}
			statuses[index] = status
		}(index, u)
	}
	wg.Wait()
	return Observation{Services: statuses, At: time.Now()}
}

type latest struct {
	sync.RWMutex
	observation Observation
	ready       bool
}

func (l *latest) put(o Observation) { l.Lock(); l.observation = o; l.ready = true; l.Unlock() }
func (l *latest) get() (Observation, bool) {
	l.RLock()
	defer l.RUnlock()
	return l.observation, l.ready
}
func poll(ctx context.Context, s source, c Config, l *latest) {
	for {
		l.put(observe(ctx, s, c))
		timer := time.NewTimer(milliseconds(c.PollMS))
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
	}
}
