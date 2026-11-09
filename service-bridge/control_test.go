package main

import (
	"context"
	"encoding/json"
	"errors"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

type fakeExecutor struct {
	calls atomic.Int32
	err   error
}

func (e *fakeExecutor) Execute(context.Context, Unit, string) error { e.calls.Add(1); return e.err }
func controlFixture(t *testing.T) (*controller, *fakeExecutor, Control) {
	t.Helper()
	config := Config{StateDirectory: t.TempDir(), Units: []Unit{{"navigation.service", "user"}}, Controls: map[string][]string{"navigation.service": {"start", "stop", "restart"}}}
	executor := &fakeExecutor{}
	controller, err := newController(config, executor)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(controller.Close)
	now := time.Now().UnixMilli()
	return controller, executor, Control{"robot.service-control.v1", "request-1", now, now + 30000, "navigation.service", "restart"}
}
func TestControlIsDurableAndIdempotentAfterRestart(t *testing.T) {
	c, e, command := controlFixture(t)
	first := c.handle(context.Background(), command)
	if first.Status != "succeeded" || first.Sequence != 1 {
		t.Fatal(first)
	}
	if second := c.handle(context.Background(), command); second != first || e.calls.Load() != 1 {
		t.Fatal(second, e.calls.Load())
	}
	// Open a new controller on the same journal, as on process restart.
	c.Close()
	restarted, err := newController(c.config, e)
	if err != nil {
		t.Fatal(err)
	}
	defer restarted.Close()
	if result := restarted.handle(context.Background(), command); result != first || e.calls.Load() != 1 {
		t.Fatal(result, e.calls.Load())
	}
	command.Action = "stop"
	if result := restarted.handle(context.Background(), command); result.Status != "rejected" || e.calls.Load() != 1 {
		t.Fatal(result)
	}
}
func TestInterruptedIntentIsNeverExecutedAgain(t *testing.T) {
	c, e, command := controlFixture(t)
	entry := JournalEntry{command, ControlResult{Schema: "robot.service-control-result.v1", TS: time.Now().UnixMilli(), Status: "running"}}
	if err := c.save(filepath.Join(c.directory, command.ID+".json"), entry); err != nil {
		t.Fatal(err)
	}
	if result := c.handle(context.Background(), command); result.Status != "failed" || result.Reason == "" || e.calls.Load() != 0 {
		t.Fatal(result)
	}
}
func TestControlsRejectExpiredUnsafeProtectedAndUnadvertisedActions(t *testing.T) {
	c, e, command := controlFixture(t)
	for _, name := range []string{"../navigation.service", "other.service", "bigbot_service_bridge.service", "fleetlyhub-agent.service"} {
		bad := command
		bad.Service = name
		if r := c.handle(context.Background(), bad); r.Status != "rejected" {
			t.Fatal(r)
		}
	}
	expired := command
	expired.Issued -= 60000
	expired.Expires -= 60000
	if r := c.handle(context.Background(), expired); r.Status != "expired" {
		t.Fatal(r)
	}
	future := command
	future.Issued += 60000
	future.Expires += 60000
	if r := c.handle(context.Background(), future); r.Status != "rejected" {
		t.Fatal(r)
	}
	if e.calls.Load() != 0 {
		t.Fatal("rejected action was executed")
	}
	raw, _ := json.Marshal(command)
	if _, err := parseControl(append(raw, []byte(" {}")...)); err == nil {
		t.Fatal("trailing data accepted")
	}
	if _, err := newController(c.config, e); err == nil {
		t.Fatal("parallel journal writer permitted")
	}
}
func TestSystemdFailureCannotBeReportedAsSuccess(t *testing.T) {
	c, e, command := controlFixture(t)
	e.err = errors.New("job canceled")
	result := c.handle(context.Background(), command)
	if result.Status != "failed" || result.Reason != "job canceled" {
		t.Fatal(result)
	}
	if c.handle(context.Background(), command) != result || e.calls.Load() != 1 {
		t.Fatal("failure retried")
	}
}

func TestProtectedServicesRejectEvenExplicitControllerAllowlist(t *testing.T) {
	for _, name := range []string{"fleetlyhub-agent.service", "service-bridge.service"} {
		c, executor, command := controlFixture(t)
		c.config.Units = append(c.config.Units, Unit{name, "user"})
		c.config.Controls[name] = []string{"start", "stop", "restart"}
		command.Service = name
		if result := c.handle(context.Background(), command); result.Status != "rejected" {
			t.Fatalf("%s: %v", name, result)
		}
		if executor.calls.Load() != 0 {
			t.Fatalf("protected service %s was controlled", name)
		}
	}
}
