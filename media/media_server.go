package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"time"
)

type serverConfig struct {
	CurrentGrant  func() mediaGrant
	Bind          string
	QueueCapacity int
	MaxFrameAge   time.Duration
}

func runServer(ctx context.Context, config serverConfig, grant mediaGrant) error {
	listener, err := (&net.ListenConfig{}).Listen(ctx, "tcp", config.Bind)
	if err != nil {
		return fmt.Errorf("bind local media interface at %s: %w", config.Bind, err)
	}
	defer listener.Close()
	log.Printf("local H.264 media interface listening: bind=%s", config.Bind)
	go func() {
		<-ctx.Done()
		_ = listener.Close()
	}()

	for {
		connection, err := listener.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return fmt.Errorf("accept media producer: %w", err)
		}
		log.Printf("media producer connected: peer=%s", connection.RemoteAddr())
		if config.CurrentGrant != nil {
			grant = config.CurrentGrant()
		}
		if err := serveProducer(ctx, connection, config, grant); errors.Is(err, errPublicationRevoked) {
			log.Printf("media publication revoked; ending local media process")
			return nil
		} else if err != nil && ctx.Err() == nil {
			log.Printf("media producer session ended: %v", err)
		} else {
			log.Printf("media producer disconnected: peer=%s", connection.RemoteAddr())
		}
	}
}

func serveProducer(parent context.Context, connection net.Conn, config serverConfig, grant mediaGrant) error {
	defer connection.Close()
	if tcp, ok := connection.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	_ = connection.SetReadDeadline(time.Now().Add(5 * time.Second))
	message, err := readClientMessage(connection)
	if err != nil {
		return fmt.Errorf("read producer HELLO: %w", err)
	}
	_ = connection.SetReadDeadline(time.Time{})
	if message.Hello == nil {
		return errors.New("HELLO must be the first media protocol message")
	}
	h := *message.Hello
	if err := writeServerMessage(connection, serverMessage{Type: serverHelloAck}); err != nil {
		return err
	}
	log.Printf("media producer accepted: width=%d height=%d fps=%d bitrate_bps=%d capabilities=%d", h.Width, h.Height, h.FPS, h.BitrateBPS, h.Capabilities)

	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	queue := newFrameQueue(config.QueueCapacity, config.MaxFrameAge)
	defer queue.close()
	feedback := make(chan serverMessage, 16)
	errorsChannel := make(chan error, 3)

	go func() { errorsChannel <- writeControls(ctx, connection, feedback) }()
	go func() { errorsChannel <- readFrames(ctx, connection, h, queue, feedback) }()
	go func() { errorsChannel <- publishToLiveKit(ctx, grant, h, queue, feedback) }()

	select {
	case <-ctx.Done():
		return ctx.Err()
	case err := <-errorsChannel:
		cancel()
		queue.close()
		_ = connection.Close()
		return err
	}
}

func writeControls(ctx context.Context, writer io.Writer, feedback <-chan serverMessage) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case message := <-feedback:
			if err := writeServerMessage(writer, message); err != nil {
				return fmt.Errorf("write producer control: %w", err)
			}
		}
	}
}

func readFrames(ctx context.Context, reader io.Reader, h hello, queue *frameQueue, feedback chan<- serverMessage) error {
	var lastTimestamp uint64
	for {
		message, err := readClientMessage(reader)
		if err != nil {
			return err
		}
		if message.Frame == nil {
			return errors.New("HELLO may only be sent once")
		}
		frame := *message.Frame
		if frame.Width != h.Width || frame.Height != h.Height {
			return fmt.Errorf("frame dimensions %dx%d differ from HELLO dimensions %dx%d", frame.Width, frame.Height, h.Width, h.Height)
		}
		if lastTimestamp != 0 && frame.Timestamp <= lastTimestamp {
			return errors.New("frame timestamps must increase monotonically")
		}
		lastTimestamp = frame.Timestamp
		if queue.push(frame) == pushNeedKeyframe && h.supportsKeyframeRequests() {
			select {
			case feedback <- serverMessage{Type: serverKeyframeRequest}:
			case <-ctx.Done():
				return ctx.Err()
			default:
			}
		}
	}
}
