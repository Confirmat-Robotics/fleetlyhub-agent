package main

import (
	"context"
	"os"
	"testing"
	"time"
)

// Explicit opt-in: the caller creates a disposable user unit; never touch ROS/robot units in a test suite.
func TestLiveSystemdJobCompletion(t *testing.T) {
	if os.Getenv("FLEETLY_SYSTEMD_TEST_UNIT") != "fleetlyhub-control-test.service" {
		t.Skip("requires disposable acceptance unit")
	}
	source := newSystemdSource(time.Second)
	defer source.Close()
	unit := Unit{"fleetlyhub-control-test.service", "user"}
	for _, action := range []string{"stop", "start", "restart", "stop"} {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		err := source.Execute(ctx, unit, action)
		cancel()
		if err != nil {
			t.Fatalf("%s: %v", action, err)
		}
	}
}
