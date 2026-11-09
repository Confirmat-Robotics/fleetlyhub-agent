package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"strings"
	"sync"
	"time"
)

const (
	grantSchema  = "fleetly.media-grant.v2"
	maxGrantSize = 16 * 1024
)

type mediaGrant struct {
	Schema           string `json:"schema"`
	PublicationID    string `json:"publication_id"`
	LiveKitURL       string `json:"livekit_url"`
	ParticipantToken string `json:"participant_token"`
	ExpiresAt        int64  `json:"expires_at"`
}

func readGrant(reader *bufio.Reader, now time.Time) (mediaGrant, error) {
	data, err := reader.ReadSlice('\n')
	line := string(data)
	if err != nil && !errors.Is(err, io.EOF) {
		return mediaGrant{}, errors.New("invalid or oversized media grant line")
	}
	if len(line) == 0 {
		return mediaGrant{}, errors.New("media grant is required on standard input")
	}
	if len(line) > maxGrantSize {
		return mediaGrant{}, errors.New("media grant exceeds 16 KiB")
	}

	return parseGrant(line, now)
}

func parseGrant(line string, now time.Time) (mediaGrant, error) {
	var grant mediaGrant
	decoder := json.NewDecoder(strings.NewReader(line))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&grant); err != nil {
		return mediaGrant{}, fmt.Errorf("parse media grant: %w", err)
	}
	if decoder.Decode(new(any)) != io.EOF {
		return mediaGrant{}, errors.New("media grant contains trailing data")
	}
	if err := grant.validate(now); err != nil {
		return mediaGrant{}, err
	}
	return grant, nil
}

func (g mediaGrant) validate(now time.Time) error {
	if g.Schema != grantSchema {
		return errors.New("unsupported media grant schema")
	}
	if len(g.PublicationID) == 0 || len(g.PublicationID) > 128 {
		return errors.New("publication_id must contain 1 to 128 characters")
	}
	u, err := url.Parse(g.LiveKitURL)
	if err != nil || (u.Scheme != "ws" && u.Scheme != "wss") || u.Host == "" {
		return errors.New("livekit_url must be an absolute ws or wss URL")
	}
	if len(g.ParticipantToken) == 0 || len(g.ParticipantToken) > maxGrantSize {
		return errors.New("participant_token must contain 1 to 16384 characters")
	}
	if g.ExpiresAt <= now.UnixMilli() {
		return errors.New("media grant has expired")
	}
	return nil
}

// The lease changes without replacing the producer socket, RTP track or room.
// Its hard expiry remains active when renewal or the parent process disappears.
type grantLease struct {
	mu     sync.Mutex
	grant  mediaGrant
	timer  *time.Timer
	ctx    context.Context
	cancel context.CancelFunc
}

func newGrantLease(parent context.Context, grant mediaGrant) (context.Context, *grantLease) {
	ctx, cancel := context.WithCancel(parent)
	lease := &grantLease{grant: grant, ctx: ctx, cancel: cancel}
	lease.timer = time.AfterFunc(time.Until(time.UnixMilli(grant.ExpiresAt)), lease.expire)
	return ctx, lease
}

func (l *grantLease) expire() {
	l.mu.Lock()
	defer l.mu.Unlock()
	if remaining := time.Until(time.UnixMilli(l.grant.ExpiresAt)); remaining > 0 {
		l.timer.Reset(remaining) // A renewal won the lock before a queued timer callback.
	} else {
		l.cancel()
	}
}

func (l *grantLease) current() mediaGrant {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.grant
}

func (l *grantLease) renew(grant mediaGrant) bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	if grant.validate(time.Now()) != nil || l.ctx.Err() != nil ||
		l.grant.ExpiresAt <= time.Now().UnixMilli() ||
		grant.PublicationID != l.grant.PublicationID || grant.LiveKitURL != l.grant.LiveKitURL ||
		grant.ExpiresAt <= l.grant.ExpiresAt {
		return false
	}
	l.grant = grant
	l.timer.Reset(time.Until(time.UnixMilli(grant.ExpiresAt)))
	return true
}

func (l *grantLease) close() {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.timer.Stop()
	l.cancel()
}
