//go:build linux

package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"net/http/httptest"
	"os"
	"strings"
	"syscall"
	"testing"
	"time"
)

func testTerminal(t *testing.T) *terminal {
	t.Helper()
	term := newTerminal(t.TempDir(), []string{"PATH=/usr/bin:/bin", "LANG=C.UTF-8", "PS1=ready> ", "ENV="})
	// A plain POSIX shell: no rc files of the machine running the test.
	term.shells = [][]string{{"/bin/sh"}}
	t.Cleanup(term.close)
	return term
}

func b64(s string) string { return base64.StdEncoding.EncodeToString([]byte(s)) }

// collect reads from cursor until the output contains want.
func collect(t *testing.T, term *terminal, cursor *int64, want string) (string, terminalResult) {
	t.Helper()
	var all strings.Builder
	deadline := time.Now().Add(10 * time.Second)
	for {
		r, err := term.read(context.Background(), cursor, 200*time.Millisecond)
		if err != nil {
			t.Fatalf("read failed: %v", err)
		}
		data, err := base64.StdEncoding.DecodeString(r.Data)
		if err != nil {
			t.Fatal(err)
		}
		all.Write(data)
		next := r.Cursor
		cursor = &next
		if strings.Contains(all.String(), want) {
			return all.String(), r
		}
		if time.Now().After(deadline) {
			t.Fatalf("no %q in terminal output %q", want, all.String())
		}
	}
}

func TestTerminalRunsAnInteractiveShell(t *testing.T) {
	term := testTerminal(t)
	t.Setenv("KVMBOX_OWNER_PASSWORD", "test-only-owner-secret")
	t.Setenv("KVMBOX_AUTOMATION_KEY", "test-only-automation-secret")
	_, r := collect(t, term, nil, "ready> ")
	if !r.Running || r.Cols != 100 || r.Rows != 26 {
		t.Fatalf("unexpected state: %+v", r)
	}
	cursor := r.Cursor
	// One line typed in two requests: the session keeps its state between them.
	if _, err := term.input([]byte("x=per")); err != nil {
		t.Fatal(err)
	}
	if _, err := term.input([]byte("sist; tty >/dev/null && echo \"got-$x-$TERM-${KVMBOX_OWNER_PASSWORD-unset}-${KVMBOX_AUTOMATION_KEY-unset}\"; stty size\r")); err != nil {
		t.Fatal(err)
	}
	out, _ := collect(t, term, &cursor, "26 100")
	if !strings.Contains(out, "got-persist-xterm-256color-unset-unset") {
		t.Fatalf("no terminal, lost state or an inherited credential: %q", out)
	}
}

func TestTerminalResizeInterruptAndScrollback(t *testing.T) {
	term := testTerminal(t)
	_, r := collect(t, term, nil, "ready> ")
	cursor := r.Cursor
	if _, err := term.resize(132, 40); err != nil {
		t.Fatal(err)
	}
	term.input([]byte("stty size\r"))
	_, r = collect(t, term, &cursor, "40 132")
	cursor = r.Cursor
	// Control-C reaches the foreground job through the terminal, not a signal we send.
	term.input([]byte("sleep 600\r"))
	time.Sleep(300 * time.Millisecond)
	term.input([]byte{3})
	term.input([]byte("echo after-$?\r"))
	out, r := collect(t, term, &cursor, "after-1")
	if !strings.Contains(out, "after-130") {
		t.Fatalf("interrupt did not end the foreground job: %q", out)
	}
	// More output than the scrollback holds: memory stays bounded, a stale
	// cursor is told to start over, and the newest output is kept.
	stale := r.Cursor
	term.input([]byte("head -c 400000 /dev/zero | tr '\\0' 'z'; echo; echo end-of-flood\r"))
	deadline := time.Now().Add(10 * time.Second)
	for {
		term.mu.Lock()
		held, tail := len(term.buf), string(term.buf[max(0, len(term.buf)-200):])
		term.mu.Unlock()
		if held > terminalScrollback {
			t.Fatalf("scrollback grew to %d", held)
		}
		if strings.Contains(tail, "end-of-flood\r\n") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("flood did not finish")
		}
		time.Sleep(20 * time.Millisecond)
	}
	got, err := term.read(context.Background(), &stale, 0)
	if err != nil || !got.Reset || len(got.Data) > base64.StdEncoding.EncodedLen(terminalChunk) {
		t.Fatalf("stale cursor not reset or chunk unbounded: reset=%v len=%d err=%v", got.Reset, len(got.Data), err)
	}
	future := got.Cursor + 1<<40
	if got, err = term.read(context.Background(), &future, 0); err != nil || !got.Reset {
		t.Fatal("a cursor from another instance was not reset")
	}
}

func TestTerminalExitRespawnsAndRestartHangsUp(t *testing.T) {
	term := testTerminal(t)
	_, r := collect(t, term, nil, "ready> ")
	cursor := r.Cursor
	term.mu.Lock()
	first := term.cur.pid
	term.mu.Unlock()
	term.input([]byte("exit\r"))
	_, r = collect(t, term, &cursor, "[shell exited]")
	cursor = r.Cursor
	// The page's next reads bring a new shell, no sooner than the respawn bound.
	_, r = collect(t, term, &cursor, "ready> ")
	cursor = r.Cursor
	term.mu.Lock()
	second := term.cur.pid
	term.mu.Unlock()
	if second == first {
		t.Fatal("no new shell")
	}
	// A job that ignores the terminal's input is ended with its shell.
	term.input([]byte("sleep 600\r"))
	time.Sleep(300 * time.Millisecond)
	if _, err := term.restart(); err != nil {
		t.Fatal(err)
	}
	collect(t, term, &cursor, "ready> ")
	deadline := time.Now().Add(5 * time.Second)
	for syscall.Kill(-second, 0) == nil {
		if time.Now().After(deadline) {
			t.Fatal("the old shell's process group survived a restart")
		}
		time.Sleep(20 * time.Millisecond)
	}
	term.close()
	if _, err := term.read(context.Background(), nil, 0); err == nil {
		// The scrollback is still readable once; after it, a closed terminal refuses.
		end := term.end
		if _, err = term.read(context.Background(), &end, 0); err == nil {
			t.Fatal("closed terminal still serves")
		}
	}
	if _, err := term.input([]byte("x")); err == nil {
		t.Fatal("closed terminal accepted input")
	}
}

func TestTerminalReadWaitsAndIsCancelled(t *testing.T) {
	term := testTerminal(t)
	_, r := collect(t, term, nil, "ready> ")
	cursor := r.Cursor
	start := time.Now()
	got, err := term.read(context.Background(), &cursor, 300*time.Millisecond)
	if err != nil || got.Data != "" || got.Cursor != cursor || time.Since(start) < 250*time.Millisecond {
		t.Fatalf("idle read did not wait: %+v %v", got, err)
	}
	go func() { time.Sleep(100 * time.Millisecond); term.input([]byte("echo woke\r")) }()
	start = time.Now()
	if got, err = term.read(context.Background(), &cursor, 10*time.Second); err != nil || got.Data == "" || time.Since(start) > 5*time.Second {
		t.Fatalf("waiting read was not woken by output: %+v %v", got, err)
	}
	cursor = got.Cursor
	collect(t, term, &cursor, "woke\r\n")
	time.Sleep(300 * time.Millisecond) // the next prompt
	term.mu.Lock()
	cursor = term.end
	term.mu.Unlock()
	ctx, cancel := context.WithCancel(context.Background())
	go func() { time.Sleep(100 * time.Millisecond); cancel() }()
	start = time.Now()
	if _, err = term.read(ctx, &cursor, 10*time.Second); err != nil || time.Since(start) > 5*time.Second {
		t.Fatal("cancelled read kept waiting")
	}
}

func TestTerminalRequestsAreValidatedBeforeUse(t *testing.T) {
	term := testTerminal(t)
	for _, body := range []string{
		`{}`, `{"op":"open"}`, `{"op":"read","data":"eA=="}`, `{"op":"read","cols":80,"rows":24}`,
		`{"op":"write"}`, `{"op":"write","data":"not base64!"}`, `{"op":"write","data":"eA==","cursor":0}`,
		`{"op":"write","data":"` + base64.StdEncoding.EncodeToString(make([]byte, terminalMaxInput+1)) + `"}`,
		`{"op":"resize","cols":80}`, `{"op":"resize","cols":19,"rows":24}`, `{"op":"resize","cols":80,"rows":201}`,
		`{"op":"resize","cols":80,"rows":24,"wait_ms":1}`, `{"op":"restart","data":"eA=="}`,
		`{"op":"read","unknown":1}`, `{"op":"read"} {"op":"read"}`, `[]`, `{"op":"write","data":"` + b64("touch executed\r") + `"`,
	} {
		req, err := decodeTerminal(strings.NewReader(body))
		if err == nil {
			_, err = term.serve(context.Background(), req)
		}
		if err == nil {
			t.Fatalf("accepted %s", body)
		}
	}
	term.mu.Lock()
	started := term.cur != nil
	term.mu.Unlock()
	if started {
		t.Fatal("an invalid request started a shell")
	}
	if _, e := os.Stat(term.dir + "/executed"); !os.IsNotExist(e) {
		t.Fatal("an invalid request reached the shell")
	}
}

func TestTerminalHTTPContractAndLimits(t *testing.T) {
	s := testService(t)
	post := func(path, body string) (int, map[string]any) {
		w := httptest.NewRecorder()
		s.ServeHTTP(w, httptest.NewRequest("POST", path, strings.NewReader(body)))
		var v map[string]any
		if e := json.Unmarshal(w.Body.Bytes(), &v); e != nil {
			t.Fatalf("not JSON: %s", w.Body.String())
		}
		if _, ok := v["ok"].(bool); !ok || w.Header().Get("Content-Type") != "application/json" || w.Header().Get("X-Content-Type-Options") != "nosniff" {
			t.Fatalf("wrong wire shape: %s", w.Body.String())
		}
		return w.Code, v
	}
	// No terminal configured: refused, and nothing else is reachable through the route.
	if code, _ := post("/terminal", `{"op":"read"}`); code != 503 {
		t.Fatal("terminal served without a terminal")
	}
	s.term, s.termReads, s.termOps = testTerminal(t), make(chan struct{}, 2), make(chan struct{}, 2)
	for _, path := range []string{"/terminal?op=read", "/terminal/", "/terminal/read"} {
		if code, _ := post(path, `{"op":"read"}`); code != 404 {
			t.Fatal("accepted alternate route", path)
		}
	}
	if code, _ := post("/terminal", `{"op":"exec","cmd":"true"}`); code != 400 {
		t.Fatal("invalid request not refused")
	}
	code, v := post("/terminal", `{"op":"read","wait_ms":0}`)
	if code != 200 || v["ok"] != true || v["running"] != true || v["reset"] != true {
		t.Fatalf("read failed: %d %v", code, v)
	}
	if code, v = post("/terminal", `{"op":"write","data":"`+b64("echo over-http\r")+`"}`); code != 200 || v["ok"] != true {
		t.Fatalf("write failed: %d %v", code, v)
	}
	collect(t, s.term, nil, "over-http\r\n")
	if code, v = post("/terminal", `{"op":"resize","cols":90,"rows":30}`); code != 200 || v["cols"] != float64(90) || v["rows"] != float64(30) {
		t.Fatalf("resize failed: %d %v", code, v)
	}
	// Waiting reads and short operations are bounded separately, and neither
	// takes a command slot.
	s.termReads <- struct{}{}
	s.termReads <- struct{}{}
	if code, _ = post("/terminal", `{"op":"read"}`); code != 429 {
		t.Fatal("read cap not enforced")
	}
	if code, _ = post("/terminal", `{"op":"resize","cols":80,"rows":24}`); code != 200 {
		t.Fatal("waiting reads blocked an operation")
	}
	s.termOps <- struct{}{}
	s.termOps <- struct{}{}
	if code, _ = post("/terminal", `{"op":"write","data":"`+b64("touch executed\r")+`"}`); code != 429 {
		t.Fatal("operation cap not enforced")
	}
	if code, _ = post("/exec", `{"cmd":"true"}`); code != 200 {
		t.Fatal("a busy terminal took the command slots")
	}
	s.slots <- struct{}{}
	s.slots <- struct{}{}
	<-s.termReads
	if code, _ = post("/terminal", `{"op":"read"}`); code != 200 {
		t.Fatal("busy commands took the terminal's slots")
	}
	if _, e := os.Stat(s.term.dir + "/executed"); !os.IsNotExist(e) {
		t.Fatal("a refused write reached the shell")
	}
}
