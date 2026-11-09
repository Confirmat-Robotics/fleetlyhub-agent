package main

import (
	"bufio"
	"context"
	"fmt"
	"strings"
	"testing"
	"time"
)

func TestReadGrant(t *testing.T) {
	now := time.Unix(1_790_000_000, 0)
	input := `{"schema":"fleetly.media-grant.v2","publication_id":"pub-test","livekit_url":"wss://example.livekit.cloud","participant_token":"token","expires_at":1790000060000}` + "\n"
	grant, err := readGrant(bufio.NewReader(strings.NewReader(input)), now)
	if err != nil {
		t.Fatalf("readGrant() error = %v", err)
	}
	if grant.PublicationID != "pub-test" {
		t.Fatalf("PublicationID = %q", grant.PublicationID)
	}
}

func TestReadGrantRejectsExpiredAndUnknownFields(t *testing.T) {
	now := time.Unix(1_790_000_000, 0)
	tests := []string{
		`{"schema":"fleetly.media-grant.v2","publication_id":"pub-test","livekit_url":"wss://example.livekit.cloud","participant_token":"token","expires_at":1789999999000}`,
		`{"schema":"fleetly.media-grant.v2","publication_id":"pub-test","livekit_url":"wss://example.livekit.cloud","participant_token":"token","expires_at":1790000060000,"secret":"no"}`,
	}
	for _, input := range tests {
		if _, err := readGrant(bufio.NewReader(strings.NewReader(input)), now); err == nil {
			t.Fatalf("readGrant() accepted %s", input)
		}
	}
}

func TestLeaseRenewalKeepsSameContextUntilNewDeadline(t *testing.T) {
	grant := mediaGrant{Schema: grantSchema, PublicationID: "continuous", LiveKitURL: "wss://fixture", ParticipantToken: "initial", ExpiresAt: time.Now().Add(150 * time.Millisecond).UnixMilli()}
	ctx, lease := newGrantLease(context.Background(), grant)
	defer lease.close()
	renewal := grant
	renewal.ExpiresAt = time.Now().Add(400 * time.Millisecond).UnixMilli()
	renewal.ParticipantToken = "renewed"
	if !lease.renew(renewal) {
		t.Fatal("renewal rejected")
	}
	select {
	case <-ctx.Done():
		t.Fatal("renewal interrupted old connection")
	case <-time.After(200 * time.Millisecond):
	}
	if lease.current().ParticipantToken != "renewed" {
		t.Fatal("producer recovery still uses old token")
	}
	select {
	case <-ctx.Done():
	case <-time.After(time.Second):
		t.Fatal("renewed lease failed to expire")
	}
	renewal.ExpiresAt = time.Now().Add(time.Hour).UnixMilli()
	if lease.renew(renewal) {
		t.Fatal("expired publication revived")
	}
}

func TestLeaseRejectsOtherPublicationURLAndOlderGrant(t *testing.T) {
	grant := mediaGrant{Schema: grantSchema, PublicationID: "original", LiveKitURL: "wss://fixture", ParticipantToken: "token", ExpiresAt: time.Now().Add(time.Minute).UnixMilli()}
	_, lease := newGrantLease(context.Background(), grant)
	defer lease.close()
	for _, change := range []func(*mediaGrant){func(g *mediaGrant) { g.PublicationID = "different" }, func(g *mediaGrant) { g.LiveKitURL = "wss://other" }, func(g *mediaGrant) { g.ExpiresAt-- }} {
		renewal := grant
		change(&renewal)
		if lease.renew(renewal) {
			t.Fatal("unrelated or older renewal accepted")
		}
	}
}

func TestControlStreamRenewsThenStopsSameLease(t *testing.T) {
	grant := mediaGrant{Schema: grantSchema, PublicationID: "original", LiveKitURL: "wss://fixture", ParticipantToken: "token", ExpiresAt: time.Now().Add(time.Minute).UnixMilli()}
	ctx, lease := newGrantLease(context.Background(), grant)
	defer lease.close()
	renewal := grant
	renewal.ExpiresAt += 60_000
	input := fmt.Sprintf(`{"schema":"%s","publication_id":"%s","livekit_url":"%s","participant_token":"renewed","expires_at":%d}`+"\nstop\n", renewal.Schema, renewal.PublicationID, renewal.LiveKitURL, renewal.ExpiresAt)
	monitorControlStream(bufio.NewReaderSize(strings.NewReader(input), maxGrantSize), lease)
	if lease.current().ExpiresAt != renewal.ExpiresAt {
		t.Fatal("stdin renewal was not applied")
	}
	if ctx.Err() == nil {
		t.Fatal("explicit stop no longer closes publication")
	}
}

func TestGrantRejectsTrailingJSONAndOversizedControlLine(t *testing.T) {
	now := time.Now()
	input := fmt.Sprintf(`{"schema":"%s","publication_id":"original","livekit_url":"wss://fixture","participant_token":"token","expires_at":%d} {}`, grantSchema, now.Add(time.Minute).UnixMilli())
	if _, err := readGrant(bufio.NewReaderSize(strings.NewReader(input), maxGrantSize), now); err == nil {
		t.Fatal("trailing grant accepted")
	}
	if _, err := readGrant(bufio.NewReaderSize(strings.NewReader(strings.Repeat("x", maxGrantSize+1)), maxGrantSize), now); err == nil {
		t.Fatal("unbounded control accepted")
	}
}
