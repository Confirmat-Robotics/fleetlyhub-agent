package main

import (
	"context"
	"testing"
	"time"
)

func testFrame(timestamp uint64, keyframe bool) videoFrame {
	return videoFrame{Timestamp: timestamp, Keyframe: keyframe, Width: 1280, Height: 720, AccessUnit: []byte{byte(timestamp)}}
}

func TestQueueStartsAtKeyframe(t *testing.T) {
	queue := newFrameQueue(2, time.Second)
	if result := queue.push(testFrame(1, false)); result != pushNeedKeyframe {
		t.Fatalf("first delta result = %v", result)
	}
	if result := queue.push(testFrame(2, true)); result != pushRecoveredAtKeyframe {
		t.Fatalf("keyframe result = %v", result)
	}
	frame, result := queue.pop(context.Background())
	if result != popFrame || frame.Timestamp != 2 {
		t.Fatalf("pop = (%+v, %v)", frame, result)
	}
}

func TestQueueDropsStaleChain(t *testing.T) {
	queue := newFrameQueue(2, time.Millisecond)
	queue.push(testFrame(1, true))
	time.Sleep(2 * time.Millisecond)
	if _, result := queue.pop(context.Background()); result != popNeedKeyframe {
		t.Fatalf("stale pop result = %v", result)
	}
}

func TestQueueDoesNotReplayAfterOverload(t *testing.T) {
	queue := newFrameQueue(2, time.Second)
	queue.push(testFrame(1, true))
	queue.push(testFrame(2, false))
	if result := queue.push(testFrame(3, false)); result != pushNeedKeyframe {
		t.Fatalf("overload result = %v", result)
	}
	if result := queue.push(testFrame(4, false)); result != pushDroppedWhileRecovering {
		t.Fatalf("recovery delta result = %v", result)
	}
	queue.push(testFrame(5, true))
	frame, result := queue.pop(context.Background())
	if result != popFrame || frame.Timestamp != 5 {
		t.Fatalf("pop = (%+v, %v)", frame, result)
	}
}

func TestQueueRejectsStaleRecoveryKeyframe(t *testing.T) {
	queue := newFrameQueue(4, time.Second)
	queue.push(testFrame(1, true))
	queue.push(testFrame(2, true))
	for i := range queue.frames {
		queue.frames[i].receivedAt = time.Now().Add(-2 * time.Second)
	}
	if _, result := queue.pop(context.Background()); result != popNeedKeyframe {
		t.Fatalf("expired recovery keyframe was replayed: %v", result)
	}
}

func TestQueueKeepsFramesAfterFreshRecoveryKeyframe(t *testing.T) {
	queue := newFrameQueue(4, time.Second)
	queue.push(testFrame(1, true))
	queue.frames[0].receivedAt = time.Now().Add(-2 * time.Second)
	queue.push(testFrame(2, true))
	queue.push(testFrame(3, false))
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	for _, expected := range []uint64{2, 3} {
		frame, result := queue.pop(ctx)
		if result != popFrame || frame.Timestamp != expected {
			t.Fatalf("recovered chain pop = (%+v, %v), expected timestamp %d", frame, result, expected)
		}
	}
}
