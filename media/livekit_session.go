package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"math"
	"sync/atomic"
	"time"

	lksdk "github.com/livekit/server-sdk-go/v2"
	"github.com/pion/rtcp"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media"
)

var errPublicationRevoked = errors.New("media publication revoked by LiveKit")

func publicationRemoved(reason lksdk.DisconnectionReason) bool {
	return reason == lksdk.ParticipantRemoved || reason == lksdk.RoomClosed
}

func publishToLiveKit(ctx context.Context, grant mediaGrant, h hello, queue *frameQueue, feedback chan<- serverMessage) (result error) {
	sessionContext, cancelSession := context.WithCancel(ctx)
	defer cancelSession()
	var revoked atomic.Bool
	defer func() {
		if revoked.Load() {
			result = errPublicationRevoked
		}
	}()
	callbacks := &lksdk.RoomCallback{
		OnDisconnectedWithReason: func(reason lksdk.DisconnectionReason) {
			if publicationRemoved(reason) {
				revoked.Store(true)
			}
			cancelSession()
		},
	}

	room, err := lksdk.ConnectToRoomWithToken(
		grant.LiveKitURL,
		grant.ParticipantToken,
		callbacks,
		lksdk.WithAutoSubscribe(false),
	)
	if err != nil {
		return fmt.Errorf("connect to LiveKit: %w", err)
	}
	defer room.Disconnect()

	requestKeyframe := func() {
		if !h.supportsKeyframeRequests() {
			return
		}
		select {
		case feedback <- serverMessage{Type: serverKeyframeRequest}:
		default:
		}
	}
	requestRate := func(bitrate float32) {
		if !h.supportsRateControl() || bitrate < 1 {
			return
		}
		bitrate = min(bitrate, float32(math.MaxUint32))
		select {
		case feedback <- serverMessage{Type: serverRateControl, BitrateBPS: uint32(bitrate), FPS: h.FPS}:
		default:
		}
	}

	track, err := lksdk.NewLocalSampleTrack(
		webrtc.RTPCodecCapability{
			MimeType:    webrtc.MimeTypeH264,
			ClockRate:   90_000,
			SDPFmtpLine: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
		},
		lksdk.WithRTCPHandler(func(packet rtcp.Packet) {
			switch packet := packet.(type) {
			case *rtcp.PictureLossIndication, *rtcp.FullIntraRequest:
				requestKeyframe()
			case *rtcp.ReceiverEstimatedMaximumBitrate:
				requestRate(packet.Bitrate)
			}
		}),
	)
	if err != nil {
		return fmt.Errorf("create H.264 track: %w", err)
	}
	defer track.Close()

	bound := make(chan struct{}, 1)
	track.OnBind(func() {
		select {
		case bound <- struct{}{}:
		default:
		}
	})
	publication, err := room.LocalParticipant.PublishTrack(track, &lksdk.TrackPublicationOptions{
		Name:        "camera",
		VideoWidth:  int(h.Width),
		VideoHeight: int(h.Height),
	})
	if err != nil {
		return fmt.Errorf("publish H.264 track: %w", err)
	}
	log.Printf("LiveKit video track published: sid=%s", publication.SID())

	select {
	case <-bound:
	case <-sessionContext.Done():
		return errors.New("LiveKit room disconnected before the track bound")
	case <-time.After(5 * time.Second):
		return errors.New("LiveKit H.264 track did not bind within five seconds")
	}

	frameDuration := time.Second / time.Duration(h.FPS)
	for {
		frame, result := queue.pop(sessionContext)
		switch result {
		case popClosed:
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return errors.New("LiveKit room disconnected")
		case popNeedKeyframe:
			requestKeyframe()
			continue
		}
		packetTimestamp := uint32((frame.Timestamp * 90_000) / 1_000_000)
		if err := track.WriteSample(media.Sample{
			Data:            frame.AccessUnit,
			Duration:        frameDuration,
			PacketTimestamp: packetTimestamp,
		}, nil); err != nil {
			return fmt.Errorf("publish H.264 frame: %w", err)
		}
	}
}
