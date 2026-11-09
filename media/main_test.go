package main

import (
	"testing"
	"time"
)

func TestValidateConfig(t *testing.T) {
	if err := validateConfig("127.0.0.1:7002", 2, 500*time.Millisecond); err != nil {
		t.Fatalf("valid config rejected: %v", err)
	}
	if err := validateConfig("0.0.0.0:7002", 2, 500*time.Millisecond); err == nil {
		t.Fatal("public bind was accepted")
	}
}
