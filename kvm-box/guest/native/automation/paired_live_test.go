//go:build linux && sunshine_live

package main

import (
	"net"
	"net/http"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// TestLiveServePaired answers the paired-records request from the state file
// of a running Sunshine, for the inner gateway's live test (inner/live_test.go
// describes the run). It serves until the stop file appears.
func TestLiveServePaired(t *testing.T) {
	dir, state, stop := os.Getenv("KVMBOX_LIVE_SOCKET_DIR"), os.Getenv("KVMBOX_LIVE_STATE"), os.Getenv("KVMBOX_LIVE_STOP")
	if dir == "" || state == "" || stop == "" {
		t.Skip("not a live run")
	}
	if err := os.Mkdir(dir, 0750); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(dir, 0750); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "desktop.sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	if err = os.Chmod(path, 0660); err != nil {
		t.Fatal(err)
	}
	s := testService(t)
	s.statePath = state
	go http.Serve(ln, s)
	for deadline := time.Now().Add(5 * time.Minute); time.Now().Before(deadline); time.Sleep(100 * time.Millisecond) {
		if _, err := os.Stat(stop); err == nil {
			// What the file held at the end, as this service reads it.
			clients, err := readPaired(state)
			if err != nil {
				t.Fatal("state file unreadable at the end:", err)
			}
			seen := map[string]bool{}
			for _, c := range clients {
				if seen[c.Cert] {
					t.Fatal("a certificate has two records at the end")
				}
				seen[c.Cert] = true
			}
			t.Logf("state file at the end: %d records, %d certificates", len(clients), len(seen))
			return
		}
	}
	t.Fatal("no stop file")
}
