package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"regexp"
	"strconv"
	"time"
)

type Unit struct {
	Name string `json:"name"`
	Bus  string `json:"bus"`
}
type Config struct {
	Address        string              `json:"agent_address"`
	PollMS         int                 `json:"poll_ms"`
	HeartbeatMS    int                 `json:"heartbeat_ms"`
	TimeoutMS      int                 `json:"timeout_ms"`
	Units          []Unit              `json:"services"`
	Controls       map[string][]string `json:"controls,omitempty"`
	StateDirectory string              `json:"state_directory,omitempty"`
}

var unitName = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_.@:-]{0,119}\.service$`)
var stateName = regexp.MustCompile(`^[a-z][a-z-]{0,63}$`)

func loadConfig(reader io.Reader) (Config, error) {
	c := Config{Address: "127.0.0.1:7001", PollMS: 1000, HeartbeatMS: 5000, TimeoutMS: 2000}
	d := json.NewDecoder(io.LimitReader(reader, 65537))
	d.DisallowUnknownFields()
	if err := d.Decode(&c); err != nil {
		return c, err
	}
	var extra any
	if err := d.Decode(&extra); err != io.EOF {
		return c, fmt.Errorf("trailing configuration data")
	}
	host, port, err := net.SplitHostPort(c.Address)
	if err != nil {
		return c, fmt.Errorf("invalid agent_address")
	}
	ip := net.ParseIP(host)
	p, e := strconv.Atoi(port)
	if ip == nil || !ip.IsLoopback() || e != nil || p < 1 || p > 65535 {
		return c, fmt.Errorf("Agent must use a numeric loopback address and valid port")
	}
	if c.PollMS < 100 || c.PollMS > 2000 || c.HeartbeatMS < 100 || c.HeartbeatMS > 10000 || c.TimeoutMS < 100 || c.TimeoutMS > 2000 {
		return c, fmt.Errorf("poll_ms must be 100..2000, heartbeat_ms 100..10000, timeout_ms 100..2000")
	}
	if c.Units == nil || len(c.Units) > 64 {
		return c, fmt.Errorf("services must be an explicit array of at most 64 entries")
	}
	seen := map[string]bool{}
	for _, u := range c.Units {
		if !unitName.MatchString(u.Name) || len(u.Name) > 128 || (u.Bus != "user" && u.Bus != "system") || seen[u.Name] {
			return c, fmt.Errorf("invalid or duplicate service: %q", u.Name)
		}
		seen[u.Name] = true
	}
	for name, actions := range c.Controls {
		bus := ""
		for _, unit := range c.Units {
			if unit.Name == name {
				bus = unit.Bus
			}
		}
		if bus != "user" || name == "service-bridge.service" || name == "bigbot_service_bridge.service" || name == "fleetlyhub-agent.service" || name == "fleetly-agent.service" || len(actions) > 3 {
			return c, fmt.Errorf("controls require an explicitly monitored user unit other than Agent/bridge")
		}
		selected := map[string]bool{}
		for _, action := range actions {
			if (action != "start" && action != "stop" && action != "restart") || selected[action] {
				return c, fmt.Errorf("invalid control action")
			}
			selected[action] = true
		}
	}
	return c, nil
}
func milliseconds(value int) time.Duration { return time.Duration(value) * time.Millisecond }
func readConfig(path string) (Config, error) {
	f, e := os.Open(path)
	if e != nil {
		return Config{}, e
	}
	defer f.Close()
	return loadConfig(f)
}
