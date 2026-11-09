package main

import (
	"bufio"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"os"
	"os/signal"
	"strings"
	"time"
)

const version = "0.2.0"

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}

func run() error {
	inputBind := flag.String("input-bind", "127.0.0.1:7002", "loopback endpoint for framed H.264 access units")
	queueCapacity := flag.Int("queue-capacity", 2, "maximum number of encoded frames waiting for LiveKit")
	maxFrameAge := flag.Duration("max-frame-age", 500*time.Millisecond, "maximum local age of an encoded frame")
	showVersion := flag.Bool("version", false, "print version and exit")
	flag.Parse()
	if *showVersion {
		fmt.Println(version)
		return nil
	}
	if err := validateConfig(*inputBind, *queueCapacity, *maxFrameAge); err != nil {
		return err
	}

	stdin := bufio.NewReaderSize(os.Stdin, maxGrantSize)
	grant, err := readGrant(stdin, time.Now())
	if err != nil {
		return err
	}
	baseContext, stopSignal := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stopSignal()
	ctx, lease := newGrantLease(baseContext, grant)
	defer lease.close()

	go monitorControlStream(stdin, lease)
	return runServer(ctx, serverConfig{
		Bind:          *inputBind,
		CurrentGrant:  lease.current,
		QueueCapacity: *queueCapacity,
		MaxFrameAge:   *maxFrameAge,
	}, grant)
}

func validateConfig(bind string, queueCapacity int, maxFrameAge time.Duration) error {
	address, err := net.ResolveTCPAddr("tcp", bind)
	if err != nil {
		return fmt.Errorf("invalid input-bind: %w", err)
	}
	if address.IP == nil || !address.IP.IsLoopback() {
		return errors.New("input-bind must use a loopback address")
	}
	if queueCapacity < 1 || queueCapacity > 8 {
		return errors.New("queue-capacity must be between 1 and 8 frames")
	}
	if maxFrameAge < 50*time.Millisecond || maxFrameAge > 5*time.Second {
		return errors.New("max-frame-age must be between 50ms and 5s")
	}
	return nil
}

func monitorControlStream(reader *bufio.Reader, lease *grantLease) {
	defer lease.close()
	for {
		line, err := reader.ReadSlice('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			log.Printf("media control stream ended or exceeded its size limit")
			return
		}
		command := strings.TrimSpace(string(line))
		if command == "stop" || command == "" {
			return
		}
		grant, parseError := parseGrant(command, time.Now())
		if parseError != nil || !lease.renew(grant) {
			log.Printf("media renewal ignored: invalid, stale or different publication")
		} else {
			log.Printf("media grant renewed: publication_id=%s expires_at=%d", grant.PublicationID, grant.ExpiresAt)
		}
		if err != nil {
			return
		}
	}
}
