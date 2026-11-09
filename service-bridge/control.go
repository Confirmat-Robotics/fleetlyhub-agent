package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"sync"
	"time"
)

type Control struct {
	Schema  string `json:"schema"`
	ID      string `json:"command_id"`
	Issued  int64  `json:"issued_at"`
	Expires int64  `json:"expires_at"`
	Service string `json:"service"`
	Action  string `json:"action"`
}
type ControlResult struct {
	Schema   string `json:"schema"`
	TS       int64  `json:"ts"`
	Sequence int64  `json:"result_seq"`
	Status   string `json:"status"`
	Reason   string `json:"reason,omitempty"`
}
type JournalEntry struct {
	Command Control       `json:"command"`
	Result  ControlResult `json:"result"`
}
type executor interface {
	Execute(context.Context, Unit, string) error
}
type controller struct {
	mu        sync.Mutex
	config    Config
	directory string
	executor  executor
	lock      *os.File
}

var controlID = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)

func parseControl(raw []byte) (Control, error) {
	var c Control
	if len(raw) > 16384 {
		return c, fmt.Errorf("oversized control")
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&c); err != nil {
		return c, err
	}
	var extra any
	if decoder.Decode(&extra) != io.EOF || c.Schema != "robot.service-control.v1" || !controlID.MatchString(c.ID) || !unitName.MatchString(c.Service) || len(c.Service) > 128 || (c.Action != "start" && c.Action != "stop" && c.Action != "restart") || c.Issued <= 0 || c.Expires < c.Issued || c.Expires-c.Issued > 30000 || c.Expires > 9007199254740991 {
		return c, fmt.Errorf("invalid control")
	}
	return c, nil
}
func newController(c Config, e executor) (*controller, error) {
	directory := c.StateDirectory
	if directory == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return nil, err
		}
		directory = filepath.Join(home, ".local", "state", "fleetlyhub-service-bridge")
	}
	if !filepath.IsAbs(directory) {
		return nil, fmt.Errorf("state_directory must be absolute")
	}
	if err := os.MkdirAll(directory, 0700); err != nil {
		return nil, err
	}
	lock, err := lockJournal(filepath.Join(directory, ".lock"))
	if err != nil {
		return nil, err
	}
	return &controller{config: c, directory: directory, executor: e, lock: lock}, nil
}
func (c *controller) Close() { c.lock.Close() }
func (c *controller) save(path string, entry JournalEntry) error {
	data, err := json.Marshal(entry)
	if err != nil {
		return err
	}
	file, err := os.CreateTemp(c.directory, ".pending-")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	defer file.Close()
	if err = file.Chmod(0600); err != nil {
		return err
	}
	if _, err = file.Write(data); err != nil {
		return err
	}
	if err = file.Sync(); err != nil {
		return err
	}
	if err = file.Close(); err != nil {
		return err
	}
	if err = os.Rename(file.Name(), path); err != nil {
		return err
	}
	dir, err := os.Open(c.directory)
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}
func (c *controller) handle(ctx context.Context, command Control) ControlResult {
	c.mu.Lock()
	defer c.mu.Unlock()
	result := ControlResult{Schema: "robot.service-control-result.v1", TS: time.Now().UnixMilli(), Sequence: 0, Status: "rejected"}
	// Defense in depth when called without the transport parser (including tests).
	raw, _ := json.Marshal(command)
	if _, err := parseControl(raw); err != nil {
		result.Reason = "Invalid control"
		return result
	}
	var unit Unit
	permitted := false
	for _, u := range c.config.Units {
		if u.Name == command.Service {
			unit = u
		}
	}
	if unit.Bus == "user" && unit.Name != "service-bridge.service" && unit.Name != "bigbot_service_bridge.service" && unit.Name != "fleetlyhub-agent.service" && unit.Name != "fleetly-agent.service" {
		for _, action := range c.config.Controls[unit.Name] {
			if action == command.Action {
				permitted = true
			}
		}
	}
	if !permitted {
		result.Reason = "Service/action is not allowlisted"
		return result
	}
	path := filepath.Join(c.directory, command.ID+".json")
	if data, err := os.ReadFile(path); err == nil {
		var old JournalEntry
		if json.Unmarshal(data, &old) != nil {
			result.Reason = "Journal unreadable; action not repeated"
			return result
		}
		if old.Command != command {
			result.Reason = "Conflicting command ID"
			return result
		}
		if old.Result.Status == "running" {
			old.Result.Status = "failed"
			old.Result.Reason = "Execution interrupted; outcome unknown, action not repeated"
			old.Result.Sequence++
			old.Result.TS = time.Now().UnixMilli()
			if c.save(path, old) != nil {
				result.Reason = "Journal write failed"
				return result
			}
		}
		return old.Result
	} else if !os.IsNotExist(err) {
		result.Reason = "Journal unavailable"
		return result
	}
	now := time.Now().UnixMilli()
	if command.Expires <= now {
		result.Status = "expired"
		result.Reason = "Command expired before dispatch"
		return result
	}
	if command.Issued > now+5000 {
		result.Reason = "Command clock is in the future"
		return result
	}
	entries, err := os.ReadDir(c.directory)
	if err != nil {
		result.Reason = "Journal unavailable"
		return result
	}
	count := 0
	for _, entry := range entries {
		if filepath.Ext(entry.Name()) != ".json" {
			continue
		}
		count++
		if info, e := entry.Info(); e == nil && time.Since(info.ModTime()) > 24*time.Hour {
			if os.Remove(filepath.Join(c.directory, entry.Name())) == nil {
				count--
			}
		}
	}
	if count >= 10000 {
		result.Reason = "Journal capacity reached"
		return result
	}
	result.Status = "running"
	if c.save(path, JournalEntry{command, result}) != nil {
		result.Status = "rejected"
		result.Reason = "Cannot persist command intent"
		return result
	}
	// This durable intent precedes D-Bus. A crash/uncertain response never triggers automatic redispatch.
	deadline, cancel := context.WithTimeout(ctx, 45*time.Second)
	defer cancel()
	err = c.executor.Execute(deadline, unit, command.Action)
	result.Sequence++
	result.TS = time.Now().UnixMilli()
	result.Status = "succeeded"
	if err != nil {
		result.Status = "failed"
		result.Reason = err.Error()
		if len(result.Reason) > 512 {
			result.Reason = result.Reason[:512]
		}
	}
	if c.save(path, JournalEntry{command, result}) != nil {
		result.Status = "failed"
		result.Reason = "Execution outcome could not be persisted; do not repeat automatically"
	}
	return result
}
