//go:build linux

package main

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

func testService(t *testing.T) *service {
	t.Helper()
	return &service{dir: t.TempDir(), env: []string{"PATH=/usr/bin:/bin", "LANG=C.UTF-8"}, slots: make(chan struct{}, 2)}
}
func intp(v int) *int { return &v }

func TestExecCompatibility(t *testing.T) {
	s := testService(t)
	for _, tc := range []struct {
		cmd  string
		code int
		text string
	}{
		{"printf 'hello\\n'; printf 'error\\n' >&2", 0, "hello\nerror\n"},
		{"printf failed; exit 7", 7, "failed"},
		{"printf '%s' 'quotes $PATH `date` and newline\nremain literal'", 0, "quotes $PATH `date` and newline\nremain literal"},
	} {
		r := execute(context.Background(), execRequest{Cmd: tc.cmd}, s.dir, s.env)
		if !r.OK || r.ExitCode == nil || *r.ExitCode != tc.code || r.Output != tc.text || r.Truncated {
			t.Fatalf("unexpected result: %+v", r)
		}
	}
}

func TestOutputCapDrainsAndDoesNotInheritSecrets(t *testing.T) {
	s := testService(t)
	t.Setenv("KVMBOX_OWNER_PASSWORD", "test-only-owner-secret")
	t.Setenv("KVMBOX_AUTOMATION_KEY", "test-only-automation-secret")
	r := execute(context.Background(), execRequest{Cmd: "printf '%s' \"${KVMBOX_OWNER_PASSWORD-unset}:${KVMBOX_AUTOMATION_KEY-unset}\""}, s.dir, s.env)
	if r.Output != "unset:unset" {
		t.Fatalf("inherited a credential: %+v", r)
	}
	r = execute(context.Background(), execRequest{Cmd: "head -c 262144 /dev/zero; printf done", MaxBytes: intp(1024)}, s.dir, s.env)
	if !r.OK || !r.Truncated || len(r.Output) != 1024 {
		t.Fatalf("unbounded or undrained output: %+v", r)
	}
}

func TestTimeoutKillsDescendant(t *testing.T) {
	s := testService(t)
	r := execute(context.Background(), execRequest{Cmd: "sleep 2; printf escaped > escaped", Timeout: intp(1)}, s.dir, s.env)
	if r.OK || r.Error == "" || r.MS > 1800 {
		t.Fatalf("timeout not bounded: %+v", r)
	}
	time.Sleep(1200 * time.Millisecond)
	if _, e := os.Stat(filepath.Join(s.dir, "escaped")); !os.IsNotExist(e) {
		t.Fatal("timed-out child escaped its process group")
	}
}

func TestClientCancellation(t *testing.T) {
	s := testService(t)
	ctx, cancel := context.WithCancel(context.Background())
	go func() { time.Sleep(50 * time.Millisecond); cancel() }()
	r := execute(ctx, execRequest{Cmd: "sleep 30"}, s.dir, s.env)
	if r.OK || r.MS > 1000 || r.Error == "" {
		t.Fatalf("cancel did not stop command: %+v", r)
	}
}

func TestDescendantHoldingPipeIsBounded(t *testing.T) {
	s := testService(t)
	r := execute(context.Background(), execRequest{Cmd: "sleep 30 &"}, s.dir, s.env)
	if r.OK || r.MS > 1500 || !strings.Contains(r.Error, "descendants") {
		t.Fatalf("pipe wait not bounded: %+v", r)
	}
}

func TestInvalidRequestsNeverExecute(t *testing.T) {
	s := testService(t)
	for _, body := range []string{
		`{"cmd":"touch executed","unexpected":true}`,
		`{"cmd":"touch executed"} {}`,
		`{"cmd":"touch executed","timeout_s":"1"}`,
		`{"cmd":"touch executed\u0000"}`,
		`{"cmd":"touch executed` + strings.Repeat(" ", maxRequest) + `"}`,
		`{"cmd":""}`,
	} {
		w := httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", "/exec", strings.NewReader(body)))
		if w.Code != 400 {
			t.Fatalf("malformed request status %d", w.Code)
		}
	}
	if _, e := os.Stat(filepath.Join(s.dir, "executed")); !os.IsNotExist(e) {
		t.Fatal("invalid request ran a command")
	}
}

func TestHTTPContractAndLimits(t *testing.T) {
	s := testService(t)
	w := httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/exec", strings.NewReader(`{"cmd":"printf native; exit 4"}`)))
	var r execResult
	if e := json.Unmarshal(w.Body.Bytes(), &r); e != nil {
		t.Fatal(e)
	}
	if w.Code != 200 || !r.OK || *r.ExitCode != 4 || r.Output != "native" {
		t.Fatalf("wrong wire response: %s", w.Body.String())
	}
	if w.Header().Get("Content-Type") != "application/json" || w.Header().Get("X-Content-Type-Options") != "nosniff" {
		t.Fatal("missing content isolation")
	}
	for _, path := range []string{"/exec?cmd=bad", "/exec/", "/unknown"} {
		w = httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", path, strings.NewReader(`{"cmd":"true"}`)))
		if w.Code != 404 {
			t.Fatal("accepted alternate route", path)
		}
	}
	s.slots <- struct{}{}
	s.slots <- struct{}{}
	w = httptest.NewRecorder()
	s.ServeHTTP(w, httptest.NewRequest("POST", "/exec", strings.NewReader(`{"cmd":"touch executed"}`)))
	if w.Code != 429 {
		t.Fatal("concurrency cap not enforced")
	}
}

func TestUnixPeerIdentity(t *testing.T) {
	for _, allowed := range []bool{true, false} {
		t.Run(strconv.FormatBool(allowed), func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "a.sock")
			ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: path, Net: "unix"})
			if err != nil {
				t.Fatal(err)
			}
			uid := uint32(os.Getuid())
			if !allowed {
				uid++
			}
			srv := &http.Server{Handler: testService(t)}
			defer srv.Close()
			go srv.Serve(&peerListener{UnixListener: ln, uid: uid, slots: make(chan struct{}, 4)})
			client := &http.Client{Timeout: time.Second, Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, "unix", path)
			}}}
			defer client.CloseIdleConnections()
			resp, err := client.Post("http://desktop/exec", "application/json", strings.NewReader(`{"cmd":"printf authenticated-peer"}`))
			if !allowed {
				if err == nil {
					resp.Body.Close()
					t.Fatal("wrong peer admitted")
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			defer resp.Body.Close()
			b, err := io.ReadAll(resp.Body)
			if err != nil || !strings.Contains(string(b), "authenticated-peer") {
				t.Fatalf("peer request failed: %s %v", b, err)
			}
		})
	}
}

func TestStartupRejectsWrongRoleAndCredentials(t *testing.T) {
	lookup := func(string) (string, bool) { return "", false }
	if e := startupCheck(1000, 1000, []string{"automation"}, lookup); e != nil {
		t.Fatal(e)
	}
	for _, uid := range []int{0, 1001, 1002, 1003} {
		if startupCheck(uid, uid, []string{"automation"}, lookup) == nil {
			t.Fatal("wrong role accepted", uid)
		}
	}
	if startupCheck(1000, 0, []string{"automation"}, lookup) == nil {
		t.Fatal("root effective identity accepted")
	}
	for _, key := range []string{"KVMBOX_OWNER_PASSWORD", "KVMBOX_ACCESS_SHA256", "KVMBOX_AUTOMATION_KEY"} {
		if startupCheck(1000, 1000, []string{"automation"}, func(k string) (string, bool) { return "", k == key }) == nil {
			t.Fatal("inherited key accepted", key)
		}
	}
	if syscall.Getuid() != os.Getuid() {
		t.Fatal("test identity mismatch")
	}
}
