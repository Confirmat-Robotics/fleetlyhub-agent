package main

import (
	"context"
	"sync"
	"time"
)

type pushResult int

const (
	pushEnqueued pushResult = iota
	pushRecoveredAtKeyframe
	pushNeedKeyframe
	pushDroppedWhileRecovering
	pushClosed
)

type popResult int

const (
	popFrame popResult = iota
	popNeedKeyframe
	popClosed
)

type queuedFrame struct {
	receivedAt time.Time
	frame      videoFrame
}

type frameQueue struct {
	mu                  sync.Mutex
	frames              []queuedFrame
	capacity            int
	maxAge              time.Duration
	waitingForKeyframe  bool
	keyframeRequestSent bool
	closed              bool
	changed             chan struct{}
}

func newFrameQueue(capacity int, maxAge time.Duration) *frameQueue {
	return &frameQueue{
		capacity:           capacity,
		maxAge:             maxAge,
		waitingForKeyframe: true,
		changed:            make(chan struct{}, 1),
	}
}

func (q *frameQueue) signal() {
	select {
	case q.changed <- struct{}{}:
	default:
	}
}

func (q *frameQueue) push(frame videoFrame) pushResult {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.closed {
		return pushClosed
	}
	if q.waitingForKeyframe {
		if !frame.Keyframe {
			if q.keyframeRequestSent {
				return pushDroppedWhileRecovering
			}
			q.keyframeRequestSent = true
			return pushNeedKeyframe
		}
		q.frames = q.frames[:0]
		q.waitingForKeyframe = false
		q.keyframeRequestSent = false
		q.frames = append(q.frames, queuedFrame{receivedAt: time.Now(), frame: frame})
		q.signal()
		return pushRecoveredAtKeyframe
	}
	if len(q.frames) == q.capacity {
		q.frames = q.frames[:0]
		if frame.Keyframe {
			q.keyframeRequestSent = false
			q.frames = append(q.frames, queuedFrame{receivedAt: time.Now(), frame: frame})
			q.signal()
			return pushRecoveredAtKeyframe
		}
		q.waitingForKeyframe = true
		q.keyframeRequestSent = true
		return pushNeedKeyframe
	}
	q.frames = append(q.frames, queuedFrame{receivedAt: time.Now(), frame: frame})
	q.signal()
	return pushEnqueued
}

func (q *frameQueue) pop(ctx context.Context) (videoFrame, popResult) {
	for {
		q.mu.Lock()
		if q.closed {
			q.mu.Unlock()
			return videoFrame{}, popClosed
		}
		if len(q.frames) > 0 {
			queued := q.frames[0]
			q.frames = q.frames[1:]
			if time.Since(queued.receivedAt) <= q.maxAge {
				q.mu.Unlock()
				return queued.frame, popFrame
			}
			for i := len(q.frames) - 1; i >= 0; i-- {
				if q.frames[i].frame.Keyframe && time.Since(q.frames[i].receivedAt) <= q.maxAge {
					frame := q.frames[i].frame
					q.frames = q.frames[i+1:] // Keep the fresh dependent frames after this keyframe.
					q.waitingForKeyframe = false
					q.keyframeRequestSent = false
					q.mu.Unlock()
					return frame, popFrame
				}
			}
			q.frames = q.frames[:0]
			q.waitingForKeyframe = true
			q.keyframeRequestSent = true
			q.mu.Unlock()
			return videoFrame{}, popNeedKeyframe
		}
		q.mu.Unlock()
		select {
		case <-ctx.Done():
			return videoFrame{}, popClosed
		case <-q.changed:
		}
	}
}

func (q *frameQueue) close() {
	q.mu.Lock()
	q.closed = true
	q.mu.Unlock()
	q.signal()
}
