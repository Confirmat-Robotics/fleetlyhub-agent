package main

import (
	"os"
	"strings"
	"testing"
)

func TestAIServiceGuideUsesAcceptedBridgeConfiguration(t *testing.T) {
	data, err := os.ReadFile("../docs/ai/services.md")
	if err != nil {
		t.Fatal(err)
	}
	text := strings.ReplaceAll(string(data), "\r\n", "\n")
	_, rest, found := strings.Cut(text, "```json service-bridge-config\n")
	if !found {
		t.Fatal("missing documented bridge configuration")
	}
	example, _, found := strings.Cut(rest, "\n```")
	if !found {
		t.Fatal("unterminated bridge configuration example")
	}
	if _, err := loadConfig(strings.NewReader(example)); err != nil {
		t.Fatalf("AI guide configuration rejected by bridge: %v", err)
	}
}
