package main

import (
	"context"
	"encoding/json"
	"flag"
	"log"
	"os"
	"os/signal"
	"syscall"
)

func main() {
	config := flag.String("config", "services.json", "Explicit monitored services configuration")
	once := flag.Bool("once", false, "Read D-Bus once and print the local projection without connecting to Agent")
	flag.Parse()
	c, err := readConfig(*config)
	if err != nil {
		log.Fatal(err)
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	source := newSystemdSource(milliseconds(c.TimeoutMS))
	defer source.Close()
	if *once {
		o := observe(ctx, source, c)
		if err = json.NewEncoder(os.Stdout).Encode(Snapshot{"robot.services-status.v1", o.At.UnixMilli(), o.Services}); err != nil {
			log.Fatal(err)
		}
		return
	}
	current := &latest{}
	var controls *controller
	if len(c.Controls) > 0 {
		controls, err = newController(c, source)
		if err != nil {
			log.Fatal(err)
		}
		defer controls.Close()
	}
	finished := make(chan struct{})
	go func() { defer close(finished); poll(ctx, source, c, current) }()
	run(ctx, c, current, controls)
	cancel()
	<-finished
}
