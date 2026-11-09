package main

import (
	"context"
	"fmt"
	"github.com/godbus/dbus/v5"
)

func (s *systemdSource) Execute(ctx context.Context, unit Unit, action string) error {
	method := map[string]string{"start": "StartUnit", "stop": "StopUnit", "restart": "RestartUnit"}[action]
	if method == "" || unit.Bus != "user" {
		return fmt.Errorf("action/bus is not permitted")
	}
	conn, err := s.buses[unit.Bus].connect(ctx)
	if err != nil {
		return err
	}
	signals := make(chan *dbus.Signal, 128)
	conn.Signal(signals)
	defer conn.RemoveSignal(signals)
	options := []dbus.MatchOption{dbus.WithMatchObjectPath("/org/freedesktop/systemd1"), dbus.WithMatchInterface("org.freedesktop.systemd1.Manager"), dbus.WithMatchMember("JobRemoved")}
	if err = conn.AddMatchSignalContext(ctx, options...); err != nil {
		return err
	}
	defer conn.RemoveMatchSignal(options...)
	manager := conn.Object("org.freedesktop.systemd1", "/org/freedesktop/systemd1")
	if call := manager.CallWithContext(ctx, "org.freedesktop.systemd1.Manager.Subscribe", 0); call.Err != nil {
		// This connection may already be subscribed by an earlier command.
		if e, ok := call.Err.(dbus.Error); !ok || e.Name != "org.freedesktop.systemd1.AlreadySubscribed" {
			return call.Err
		}
	}
	var job dbus.ObjectPath
	if err = manager.CallWithContext(ctx, "org.freedesktop.systemd1.Manager."+method, 0, unit.Name, "replace").Store(&job); err != nil {
		return err
	}
	for {
		select {
		case <-ctx.Done():
			return fmt.Errorf("systemd completion timed out; execution outcome unknown")
		case signal, ok := <-signals:
			if !ok {
				return fmt.Errorf("systemd connection lost; execution outcome unknown")
			}
			if len(signal.Body) != 4 || signal.Body[1] != job || signal.Body[2] != unit.Name {
				continue
			}
			outcome, ok := signal.Body[3].(string)
			if !ok || outcome != "done" {
				return fmt.Errorf("systemd job did not succeed: %v", signal.Body[3])
			}
			final, err := s.Read(ctx, unit)
			if err != nil {
				return fmt.Errorf("systemd job finished but final state could not be read")
			}
			expected := "active"
			if action == "stop" {
				expected = "inactive"
			}
			if final.Status != expected {
				return fmt.Errorf("systemd job finished with unexpected state: %s/%s", final.Status, final.Substatus)
			}
			return nil
		}
	}
}
