//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

// RFC 6455 section 1.3 example, computed independently of wsAccept.
const (
	rfcKey    = "dGhlIHNhbXBsZSBub25jZQ=="
	rfcAccept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
)

// ---- control socket fixture (stands in for the root supervisor) ----

type ctlFixture struct {
	dir, path string
	gid       uint32
	ln        *net.UnixListener
	reply     []byte
	hang      bool
	done      chan struct{}
	accepts   atomic.Int64
	handled   atomic.Int64
	mu        sync.Mutex
	cmds      [][]byte
}

func statOf(t *testing.T, p string) *syscall.Stat_t {
	t.Helper()
	fi, err := os.Lstat(p)
	if err != nil {
		t.Fatal(err)
	}
	return fi.Sys().(*syscall.Stat_t)
}

func startCtl(t *testing.T, reply []byte, hang bool) *ctlFixture {
	t.Helper()
	dir, err := os.MkdirTemp("", "ncg")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	if err := os.Chmod(dir, 0o750); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "control.sock")
	ln, err := net.ListenUnix("unix", &net.UnixAddr{Name: path, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	f := &ctlFixture{dir: dir, path: path, ln: ln, reply: reply, hang: hang, done: make(chan struct{})}
	f.gid = statOf(t, dir).Gid
	if err := os.Chown(path, -1, int(f.gid)); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o660); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		close(f.done)
		ln.Close()
	})
	go f.serve()
	return f
}

func (f *ctlFixture) serve() {
	for {
		c, err := f.ln.Accept()
		if err != nil {
			return
		}
		f.accepts.Add(1)
		go func() {
			defer f.handled.Add(1)
			defer c.Close()
			c.SetDeadline(time.Now().Add(5 * time.Second))
			data, _ := io.ReadAll(io.LimitReader(c, 64))
			if len(data) > 0 {
				f.mu.Lock()
				f.cmds = append(f.cmds, data)
				f.mu.Unlock()
			}
			if f.hang {
				select {
				case <-f.done:
				case <-time.After(5 * time.Second):
				}
				return
			}
			c.Write(f.reply)
		}()
	}
}

func (f *ctlFixture) commands(t *testing.T) []string {
	t.Helper()
	eventually(t, "control handlers", func() bool { return f.handled.Load() == f.accepts.Load() })
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []string
	for _, c := range f.cmds {
		out = append(out, string(c))
	}
	return out
}

func frame(s string) []byte {
	b := make([]byte, 4+len(s))
	binary.BigEndian.PutUint32(b, uint32(len(s)))
	copy(b[4:], s)
	return b
}

// ---- inner desktop gateway fixture ----

type innerFixture struct {
	srv     *httptest.Server
	hits    atomic.Int64
	mu      sync.Mutex
	header  http.Header
	host    string
	uri     string
	bodyLen int64
	events  chan string
}

func startInner(t *testing.T) *innerFixture {
	t.Helper()
	f := &innerFixture{events: make(chan string, 128)}
	f.srv = httptest.NewServer(http.HandlerFunc(f.handle))
	t.Cleanup(f.srv.Close)
	return f
}

func (f *innerFixture) addr() string { return f.srv.Listener.Addr().String() }

func (f *innerFixture) event(e string) {
	select {
	case f.events <- e:
	default:
	}
}

func (f *innerFixture) seen() (http.Header, string, string, int64) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.header, f.host, f.uri, f.bodyLen
}

func (f *innerFixture) handle(w http.ResponseWriter, r *http.Request) {
	f.hits.Add(1)
	f.mu.Lock()
	f.header, f.host, f.uri = r.Header.Clone(), r.Host, r.RequestURI
	f.mu.Unlock()
	switch r.URL.Path {
	case "/websockify":
		accept := "invalid"
		if r.Header.Get("Sec-WebSocket-Key") == rfcKey {
			accept = rfcAccept
		}
		f.upgrade(w, r, "Sec-WebSocket-Accept: "+accept+"\r\n")
	case "/api/moonlight/tunnel":
		f.upgrade(w, r, "")
	case "/compute/slow":
		select {
		case <-r.Context().Done():
			f.event("cancelled")
		case <-time.After(5 * time.Second):
		}
	case "/compute/redirect":
		w.Header().Set("Location", "/vnc.html")
		w.WriteHeader(http.StatusFound)
	case "/compute/absredirect":
		w.Header().Set("Location", "http://evil.example/")
		w.WriteHeader(http.StatusFound)
	default:
		n, _ := io.Copy(io.Discard, r.Body)
		f.mu.Lock()
		f.bodyLen = n
		f.mu.Unlock()
		w.Header().Set("Set-Cookie", "inner=1")
		io.WriteString(w, "inner-ok")
	}
}

// upgrade echoes bytes; after the peer half-closes it sends EOF-SEEN and closes.
func (f *innerFixture) upgrade(w http.ResponseWriter, r *http.Request, extra string) {
	c, brw, err := w.(http.Hijacker).Hijack()
	if err != nil {
		return
	}
	defer c.Close()
	c.SetDeadline(time.Now().Add(10 * time.Second))
	fmt.Fprintf(brw, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: %s\r\n%s\r\n", r.Header.Get("Upgrade"), extra)
	if brw.Flush() != nil {
		return
	}
	buf := make([]byte, 4096)
	for {
		n, err := brw.Read(buf)
		if n > 0 {
			if _, werr := c.Write(buf[:n]); werr != nil {
				f.event("closed")
				return
			}
		}
		if err == io.EOF {
			c.Write([]byte("EOF-SEEN"))
			f.event("eof")
			return
		}
		if err != nil {
			f.event("closed")
			return
		}
	}
}

// ---- helpers ----

func testConfig(inner string, ctl *ctlFixture) config {
	cfg := productionConfig()
	cfg.innerAddr = inner
	uid := uint32(os.Getuid())
	cfg.ctlDirUID, cfg.ctlSockUID, cfg.ctlPeerUID = uid, uid, uid
	cfg.controlPath = "/nonexistent-ncg/control.sock"
	if ctl != nil {
		cfg.controlPath, cfg.ctlGID = ctl.path, ctl.gid
	}
	cfg.ctlConnect = time.Second
	cfg.ctlIO = time.Second
	cfg.upstreamDial = time.Second
	return cfg
}

func startGateway(t *testing.T, cfg config) (*gateway, string) {
	t.Helper()
	g := newGateway(cfg)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	go g.Serve(ln)
	t.Cleanup(func() { g.Close() })
	return g, ln.Addr().String()
}

func testClient() *http.Client {
	return &http.Client{
		Transport:     &http.Transport{DisableKeepAlives: true},
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
		Timeout:       15 * time.Second,
	}
}

func TestAutomationRoutesUseFixedInnerAndStripCredentials(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	c := testClient()
	defer c.CloseIdleConnections()
	for _, path := range []string{"/exec", "/computer"} {
		req, e := http.NewRequest("POST", "http://"+addr+path, strings.NewReader(`{}`))
		if e != nil {
			t.Fatal(e)
		}
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("X-Api-Key", "must-not-forward")
		r, e := c.Do(req)
		if e != nil {
			t.Fatal(e)
		}
		io.Copy(io.Discard, r.Body)
		r.Body.Close()
		if r.StatusCode != 200 {
			t.Fatal("automation route refused", r.StatusCode)
		}
		h, host, uri, n := inner.seen()
		if h.Get("X-Api-Key") != "" || host != inner.addr() || uri != path || n != 2 {
			t.Fatal("incorrect forwarding", h, host, uri, n)
		}
	}
	before := inner.hits.Load()
	for _, path := range []string{"/exec?cmd=true", "/computer/", "/execution"} {
		r, e := c.Post("http://"+addr+path, "application/json", strings.NewReader(`{}`))
		if e != nil {
			t.Fatal(e)
		}
		io.Copy(io.Discard, r.Body)
		r.Body.Close()
		if r.StatusCode == 200 {
			t.Fatal("route alias admitted", path)
		}
	}
	if inner.hits.Load() != before {
		t.Fatal("invalid route reached inner service")
	}
}

func eventually(t *testing.T, what string, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func waitEvent(t *testing.T, f *innerFixture, want ...string) {
	t.Helper()
	timeout := time.After(3 * time.Second)
	for {
		select {
		case e := <-f.events:
			for _, w := range want {
				if e == w {
					return
				}
			}
		case <-timeout:
			t.Fatalf("no %v event from inner gateway", want)
		}
	}
}

func rawRoundTrip(t *testing.T, addr, req string) (int, string) {
	t.Helper()
	c, err := net.Dial("tcp", addr)
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	c.SetDeadline(time.Now().Add(5 * time.Second))
	io.WriteString(c, req)
	resp, err := http.ReadResponse(bufio.NewReader(c), nil)
	if err != nil {
		return 0, ""
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
	return resp.StatusCode, string(b)
}

// dialUpgrade sends a raw upgrade request. On 101 it returns the open
// connection and a reader positioned after the response head.
func dialUpgrade(addr, target, headers string) (net.Conn, *bufio.Reader, *http.Response) {
	c, err := net.Dial("tcp", addr)
	if err != nil {
		return nil, nil, nil
	}
	c.SetDeadline(time.Now().Add(5 * time.Second))
	fmt.Fprintf(c, "GET %s HTTP/1.1\r\nHost: %s\r\n%s\r\n", target, addr, headers)
	br := bufio.NewReader(c)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodGet})
	if err != nil {
		c.Close()
		return nil, nil, nil
	}
	if resp.StatusCode != http.StatusSwitchingProtocols {
		c.Close()
		return nil, nil, resp
	}
	return c, br, resp
}

func status(resp *http.Response) int {
	if resp == nil {
		return 0
	}
	return resp.StatusCode
}

const tunnelHeaders = "Connection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n"

func wsHeaders(key string) string {
	return "Connection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key + "\r\n"
}

// ---- startup ----

func TestStartupRefusals(t *testing.T) {
	env := func(m map[string]string) func(string) (string, bool) {
		return func(k string) (string, bool) { v, ok := m[k]; return v, ok }
	}
	if err := startupCheck(1002, 1002, env(nil), []string{"gateway"}); err != nil {
		t.Fatalf("clean start refused: %v", err)
	}
	cases := map[string]error{
		"uid":         startupCheck(1000, 1002, env(nil), []string{"gateway"}),
		"euid":        startupCheck(1002, 0, env(nil), []string{"gateway"}),
		"root":        startupCheck(0, 0, env(nil), []string{"gateway"}),
		"password":    startupCheck(1002, 1002, env(map[string]string{"KVMBOX_OWNER_PASSWORD": ""}), []string{"gateway"}),
		"legacy hash": startupCheck(1002, 1002, env(map[string]string{"KVMBOX_ACCESS_SHA256": "00"}), []string{"gateway"}),
		"argument":    startupCheck(1002, 1002, env(nil), []string{"gateway", "-listen=0.0.0.0:1"}),
	}
	for name, err := range cases {
		if err == nil {
			t.Errorf("%s: startup accepted", name)
		}
	}
}

func TestProductionConfigIsFixed(t *testing.T) {
	c := productionConfig()
	if c.innerAddr != "127.0.0.1:18080" || c.controlPath != "/run/native-desktop/control.sock" ||
		c.ctlDirUID != 0 || c.ctlSockUID != 0 || c.ctlPeerUID != 0 || c.ctlGID != 1002 ||
		c.ctlConnect != 3*time.Second || c.ctlIO != 5*time.Second || c.maxActive != 32 {
		t.Fatalf("unexpected production config %+v", c)
	}
}

// ---- control socket ----

func TestControlRoundTripOverUnixSocket(t *testing.T) {
	const body = `{"status":"running","pid":4242,"vcpus":8,"memoryMiB":16384}`
	ctl := startCtl(t, frame(body), false)
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), ctl))
	for _, tc := range []struct{ method, path string }{
		{"GET", "/v1/status"}, {"POST", "/v1/start"}, {"POST", "/v1/stop"},
	} {
		req, _ := http.NewRequest(tc.method, "http://"+addr+tc.path, nil)
		if tc.method == "POST" {
			req.Header.Set("X-KVM-Box-Request", "1")
		}
		resp, err := testClient().Do(req)
		if err != nil {
			t.Fatal(err)
		}
		got, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if resp.StatusCode != 200 || string(got) != body {
			t.Fatalf("%s %s: %d %q", tc.method, tc.path, resp.StatusCode, got)
		}
	}
	if got := strings.Join(ctl.commands(t), ","); got != "Q,B,H" {
		t.Fatalf("commands %q, want Q,B,H", got)
	}
	if inner.hits.Load() != 0 {
		t.Fatal("control route reached inner gateway")
	}
}

func TestControlRefusesUnsafeSocketBeforeCommand(t *testing.T) {
	uid := uint32(os.Getuid())
	cases := []struct {
		name     string
		connects bool
		mutate   func(t *testing.T, f *ctlFixture, c *config)
	}{
		{"server peer uid", true, func(t *testing.T, f *ctlFixture, c *config) { c.ctlPeerUID = uid + 1 }},
		{"production peer uid", true, func(t *testing.T, f *ctlFixture, c *config) {
			if uid == 0 {
				t.Skip("running as root")
			}
			c.ctlPeerUID = rootUID
		}},
		{"socket mode", false, func(t *testing.T, f *ctlFixture, c *config) { os.Chmod(f.path, 0o666) }},
		{"socket owner", false, func(t *testing.T, f *ctlFixture, c *config) { c.ctlSockUID = uid + 1 }},
		{"socket group", false, func(t *testing.T, f *ctlFixture, c *config) { c.ctlGID = f.gid + 1 }},
		{"dir mode", false, func(t *testing.T, f *ctlFixture, c *config) { os.Chmod(f.dir, 0o755) }},
		{"dir owner", false, func(t *testing.T, f *ctlFixture, c *config) { c.ctlDirUID = uid + 1 }},
		{"symlink", false, func(t *testing.T, f *ctlFixture, c *config) {
			link := filepath.Join(f.dir, "link.sock")
			if err := os.Symlink(f.path, link); err != nil {
				t.Fatal(err)
			}
			c.controlPath = link
		}},
		{"symlinked dir", false, func(t *testing.T, f *ctlFixture, c *config) {
			link := f.dir + "-link"
			if err := os.Symlink(f.dir, link); err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { os.Remove(link) })
			c.controlPath = filepath.Join(link, "control.sock")
		}},
		{"regular file", false, func(t *testing.T, f *ctlFixture, c *config) {
			p := filepath.Join(f.dir, "file.sock")
			os.WriteFile(p, nil, 0o660)
			os.Chmod(p, 0o660)
			c.controlPath = p
		}},
		{"missing", false, func(t *testing.T, f *ctlFixture, c *config) { c.controlPath = filepath.Join(f.dir, "absent.sock") }},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctl := startCtl(t, frame(`{"status":"running","pid":1,"vcpus":4,"memoryMiB":8192}`), false)
			inner := startInner(t)
			cfg := testConfig(inner.addr(), ctl)
			tc.mutate(t, ctl, &cfg)
			_, addr := startGateway(t, cfg)
			code, body := rawRoundTrip(t, addr, "POST /v1/start HTTP/1.1\r\nHost: "+addr+"\r\nX-KVM-Box-Request: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
			if code != 503 || body != "{\"error\":\"service unavailable\"}\n" {
				t.Fatalf("got %d %q", code, body)
			}
			if cmds := ctl.commands(t); len(cmds) != 0 {
				t.Fatalf("command sent to refused server: %q", cmds)
			}
			if !tc.connects && ctl.accepts.Load() != 0 {
				t.Fatal("connected despite failed socket check")
			}
		})
	}
}

func TestControlFramingAndFields(t *testing.T) {
	const valid = `{"status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192}`
	oversize := make([]byte, 4)
	binary.BigEndian.PutUint32(oversize, maxCtlReply+1)
	oversize = append(oversize, bytes.Repeat([]byte(" "), maxCtlReply+1)...)
	cases := []struct {
		name  string
		reply []byte
		ok    bool
	}{
		{"valid", frame(valid), true},
		{"bounds", frame(`{"status":"failed","pid":2147483647,"vcpus":16,"memoryMiB":65536}`), true},
		{"zero length", []byte{0, 0, 0, 0}, false},
		{"oversize length", oversize, false},
		{"truncated length", []byte{0, 0}, false},
		{"truncated body", frame(valid)[:20], false},
		{"surplus byte", append(frame(valid), 'x'), false},
		{"second frame", append(frame(valid), frame(valid)...), false},
		{"whitespace", frame(`{"status": "stopped","pid":0,"vcpus":4,"memoryMiB":8192}`), false},
		{"trailing newline", frame(valid + "\n"), false},
		{"reordered", frame(`{"pid":0,"status":"stopped","vcpus":4,"memoryMiB":8192}`), false},
		{"missing key", frame(`{"status":"stopped","pid":0,"vcpus":4}`), false},
		{"extra key", frame(`{"status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192,"error":"x"}`), false},
		{"duplicate key", frame(`{"status":"failed","status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192}`), false},
		{"case folded key", frame(`{"Status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192}`), false},
		{"escaped value", frame(`{"status":"\u0073topped","pid":0,"vcpus":4,"memoryMiB":8192}`), false},
		{"unknown status", frame(`{"status":"paused","pid":0,"vcpus":4,"memoryMiB":8192}`), false},
		{"negative pid", frame(`{"status":"running","pid":-1,"vcpus":4,"memoryMiB":8192}`), false},
		{"float pid", frame(`{"status":"running","pid":1.0,"vcpus":4,"memoryMiB":8192}`), false},
		{"exponent", frame(`{"status":"running","pid":1,"vcpus":4,"memoryMiB":8.192e3}`), false},
		{"string vcpus", frame(`{"status":"running","pid":1,"vcpus":"4","memoryMiB":8192}`), false},
		{"vcpus low", frame(`{"status":"running","pid":1,"vcpus":0,"memoryMiB":8192}`), false},
		{"vcpus high", frame(`{"status":"running","pid":1,"vcpus":17,"memoryMiB":8192}`), false},
		{"memory low", frame(`{"status":"running","pid":1,"vcpus":4,"memoryMiB":8191}`), false},
		{"memory high", frame(`{"status":"running","pid":1,"vcpus":4,"memoryMiB":65537}`), false},
		{"null", frame(`null`), false},
		{"array", frame(`[]`), false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctl := startCtl(t, tc.reply, false)
			g := newGateway(testConfig("127.0.0.1:1", ctl))
			defer g.Close()
			st, err := g.control(context.Background(), 'Q')
			if tc.ok != (err == nil) {
				t.Fatalf("ok=%v err=%v", tc.ok, err)
			}
			if tc.ok && (st.Status == "" || st.Vcpus == 0) {
				t.Fatalf("decoded %+v", st)
			}
			if cmds := ctl.commands(t); len(cmds) != 1 || cmds[0] != "Q" {
				t.Fatalf("server received %q", cmds)
			}
		})
	}
}

func TestControlErrorIsGeneric(t *testing.T) {
	ctl := startCtl(t, frame(`{"status":"failed","pid":0,"vcpus":4,"memoryMiB":8192,"detail":"SECRET-DIAGNOSTIC"}`), false)
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), ctl))
	code, body := rawRoundTrip(t, addr, "GET /v1/status HTTP/1.1\r\nHost: "+addr+"\r\nConnection: close\r\n\r\n")
	if code != 503 || strings.Contains(body, "SECRET") || strings.Contains(body, "detail") {
		t.Fatalf("got %d %q", code, body)
	}
}

func TestControlReadTimeout(t *testing.T) {
	ctl := startCtl(t, nil, true)
	cfg := testConfig("127.0.0.1:1", ctl)
	cfg.ctlIO = 200 * time.Millisecond
	g := newGateway(cfg)
	defer g.Close()
	start := time.Now()
	if _, err := g.control(context.Background(), 'B'); err == nil {
		t.Fatal("silent server accepted")
	}
	if d := time.Since(start); d > 2*time.Second {
		t.Fatalf("timeout took %v", d)
	}
	eventually(t, "command", func() bool {
		ctl.mu.Lock()
		defer ctl.mu.Unlock()
		return len(ctl.cmds) == 1 && string(ctl.cmds[0]) == "B"
	})
}

func TestControlCancelledByRequestContext(t *testing.T) {
	ctl := startCtl(t, nil, true)
	g := newGateway(testConfig("127.0.0.1:1", ctl)) // 1s I/O timeout
	defer g.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := g.control(ctx, 'Q'); err == nil {
		t.Fatal("cancelled exchange succeeded")
	}
	if d := time.Since(start); d > 700*time.Millisecond {
		t.Fatalf("cancellation took %v", d)
	}
}

func TestControlAlreadyCancelledSendsNothing(t *testing.T) {
	ctl := startCtl(t, frame(`{"status":"stopped","pid":0,"vcpus":4,"memoryMiB":8192}`), false)
	g := newGateway(testConfig("127.0.0.1:1", ctl))
	defer g.Close()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	for i := 0; i < 32; i++ {
		if _, err := g.control(ctx, 'B'); err == nil {
			t.Fatal("cancelled action succeeded")
		}
	}
	if ctl.accepts.Load() != 0 {
		t.Fatal("already cancelled action contacted supervisor")
	}
}

func TestControlRouteValidation(t *testing.T) {
	ctl := startCtl(t, frame(`{"status":"running","pid":1,"vcpus":4,"memoryMiB":8192}`), false)
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), ctl))
	h := "Host: " + addr + "\r\nConnection: close\r\n"
	cases := []struct {
		req  string
		want int
	}{
		{"POST /v1/start HTTP/1.1\r\n" + h + "Content-Length: 0\r\n\r\n", 403},
		{"POST /v1/start HTTP/1.1\r\n" + h + "X-KVM-Box-Request: 2\r\nContent-Length: 0\r\n\r\n", 403},
		{"POST /v1/start HTTP/1.1\r\n" + h + "X-KVM-Box-Request: 1\r\nX-KVM-Box-Request: 1\r\nContent-Length: 0\r\n\r\n", 400},
		{"POST /v1/start HTTP/1.1\r\n" + h + "X-KVM-Box-Request: 1\r\nContent-Length: 1\r\n\r\nx", 400},
		{"POST /v1/start HTTP/1.1\r\n" + h + "X-KVM-Box-Request: 1\r\nUpgrade: websocket\r\nContent-Length: 0\r\n\r\n", 400},
		{"GET /v1/start HTTP/1.1\r\n" + h + "\r\n", 405},
		{"POST /v1/status HTTP/1.1\r\n" + h + "X-KVM-Box-Request: 1\r\nContent-Length: 0\r\n\r\n", 405},
		{"HEAD /v1/status HTTP/1.1\r\n" + h + "\r\n", 405},
		{"GET /v1/status?x=1 HTTP/1.1\r\n" + h + "\r\n", 400},
		{"GET /v1/status? HTTP/1.1\r\n" + h + "\r\n", 400},
		{"GET /v1/status/ HTTP/1.1\r\n" + h + "\r\n", 404},
		{"GET /V1/status HTTP/1.1\r\n" + h + "\r\n", 404},
		{"GET /v1 HTTP/1.1\r\n" + h + "\r\n", 404},
		{"GET /v1/exec HTTP/1.1\r\n" + h + "\r\n", 404},
		{"GET /v1/../v1/status HTTP/1.1\r\n" + h + "\r\n", 400},
		{"GET /v1/%73tatus HTTP/1.1\r\n" + h + "\r\n", 400},
	}
	for _, tc := range cases {
		if code, _ := rawRoundTrip(t, addr, tc.req); code != tc.want {
			t.Errorf("%q: got %d want %d", strings.SplitN(tc.req, "\r\n", 2)[0], code, tc.want)
		}
	}
	if ctl.accepts.Load() != 0 || inner.hits.Load() != 0 {
		t.Fatalf("refused requests reached control (%d) or inner (%d)", ctl.accepts.Load(), inner.hits.Load())
	}
}

// ---- HTTP proxy ----

func TestProxyForwardsAllowlistedHeadersOnly(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	req, _ := http.NewRequest("GET", "http://"+addr+"/compute/v1/models?limit=2", nil)
	ownerCred := "Basic b3duZXI6c2VjcmV0"
	for k, v := range map[string]string{
		"Authorization":           ownerCred,
		"Proxy-Authorization":     ownerCred,
		"Cookie":                  "session=1",
		"Forwarded":               "for=1.2.3.4",
		"X-Forwarded-For":         "1.2.3.4",
		"X-Forwarded-Host":        "evil.example",
		"X-Forwarded-Proto":       "https",
		"X-Real-IP":               "1.2.3.4",
		"X-Internal-Auth":         "root",
		"X-Owner-Authenticated":   "1",
		"X-Compute-Authorization": "Bearer compute-token",
		"X-KVM-Box-Request":       "1",
		"Accept":                  "application/json",
		"Origin":                  "http://" + addr,
	} {
		req.Header.Set(k, v)
	}
	resp, err := testClient().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != 200 || string(body) != "inner-ok" {
		t.Fatalf("got %d %q", resp.StatusCode, body)
	}
	if resp.Header.Get("Set-Cookie") != "" {
		t.Fatal("upstream Set-Cookie passed through")
	}
	hdr, host, uri, _ := inner.seen()
	if host != inner.addr() || uri != "/compute/v1/models?limit=2" {
		t.Fatalf("upstream host %q uri %q", host, uri)
	}
	if hdr.Get("X-Compute-Authorization") != "Bearer compute-token" || hdr.Get("X-KVM-Box-Request") != "1" || hdr.Get("Accept") != "application/json" {
		t.Fatalf("allowed headers lost: %v", hdr)
	}
	for _, k := range []string{"Authorization", "Proxy-Authorization", "Cookie", "Forwarded", "X-Forwarded-For",
		"X-Forwarded-Host", "X-Forwarded-Proto", "X-Real-Ip", "X-Internal-Auth", "X-Owner-Authenticated", "Origin"} {
		if v := hdr.Values(k); len(v) != 0 {
			t.Errorf("%s forwarded: %q", k, v)
		}
	}
	for k, vs := range hdr {
		for _, v := range vs {
			if strings.Contains(v, "b3duZXI6c2VjcmV0") {
				t.Errorf("owner credential forwarded in %s", k)
			}
		}
	}
}

func TestProxyRejectsWithoutUpstreamContact(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	h := "Host: " + addr + "\r\n"
	hc := h + "Connection: close\r\n"
	cases := map[string]string{
		"absolute url":      "GET http://" + inner.addr() + "/vnc.html HTTP/1.1\r\n" + hc + "\r\n",
		"connect":           "CONNECT " + inner.addr() + " HTTP/1.1\r\nHost: " + inner.addr() + "\r\n\r\n",
		"trace":             "TRACE /vnc.html HTTP/1.1\r\n" + hc + "\r\n",
		"put":               "PUT /compute/x HTTP/1.1\r\n" + hc + "Content-Length: 0\r\n\r\n",
		"options star":      "OPTIONS * HTTP/1.1\r\n" + hc + "\r\n",
		"dot dot":           "GET /compute/../v1/status HTTP/1.1\r\n" + hc + "\r\n",
		"dot":               "GET /compute/./x HTTP/1.1\r\n" + hc + "\r\n",
		"encoded dots":      "GET /compute/%2e%2e/v1/status HTTP/1.1\r\n" + hc + "\r\n",
		"encoded slash":     "GET /compute%2Fx HTTP/1.1\r\n" + hc + "\r\n",
		"encoded v1":        "GET /%76%31/start HTTP/1.1\r\n" + hc + "\r\n",
		"double slash":      "GET //compute/x HTTP/1.1\r\n" + hc + "\r\n",
		"empty segment":     "GET /compute//x HTTP/1.1\r\n" + hc + "\r\n",
		"backslash":         "GET /compute/a\\b HTTP/1.1\r\n" + hc + "\r\n",
		"unknown route":     "GET /etc/passwd HTTP/1.1\r\n" + hc + "\r\n",
		"root":              "GET / HTTP/1.1\r\n" + hc + "\r\n",
		"compute no slash":  "GET /compute HTTP/1.1\r\n" + hc + "\r\n",
		"semicolon query":   "GET /compute/x?a=1;b=2 HTTP/1.1\r\n" + hc + "\r\n",
		"pair get":          "GET /api/moonlight/pair HTTP/1.1\r\n" + hc + "\r\n",
		"pair query":        "POST /api/moonlight/pair?x=1 HTTP/1.1\r\n" + hc + "Content-Length: 0\r\n\r\n",
		"chunked":           "POST /compute/x HTTP/1.1\r\n" + hc + "Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
		"dup host":          "GET /vnc.html HTTP/1.1\r\n" + hc + h + "\r\n",
		"dup auth":          "GET /vnc.html HTTP/1.1\r\n" + hc + "Authorization: a\r\nAuthorization: b\r\n\r\n",
		"dup origin":        "GET /vnc.html HTTP/1.1\r\n" + hc + "Origin: http://" + addr + "\r\nOrigin: http://" + addr + "\r\n\r\n",
		"dup upgrade":       "GET /websockify HTTP/1.1\r\n" + h + "Connection: Upgrade\r\nUpgrade: websocket\r\nUpgrade: websocket\r\n\r\n",
		"dup connection":    "GET /vnc.html HTTP/1.1\r\n" + hc + "Connection: close\r\n\r\n",
		"dup compute":       "GET /compute/x HTTP/1.1\r\n" + hc + "X-Compute-Authorization: a\r\nX-Compute-Authorization: a\r\n\r\n",
		"dup action":        "POST /compute/x HTTP/1.1\r\n" + hc + "X-KVM-Box-Request: 1\r\nX-KVM-Box-Request: 1\r\nContent-Length: 0\r\n\r\n",
		"conflicting cl":    "POST /compute/x HTTP/1.1\r\n" + hc + "Content-Length: 1\r\nContent-Length: 2\r\n\r\nab",
		"foreign origin":    "GET /vnc.html HTTP/1.1\r\n" + hc + "Origin: http://evil.example\r\n\r\n",
		"null origin":       "GET /vnc.html HTTP/1.1\r\n" + hc + "Origin: null\r\n\r\n",
		"plain upgrade":     "GET /vnc.html HTTP/1.1\r\n" + h + "Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
		"large headers":     "GET /vnc.html HTTP/1.1\r\n" + hc + "X-Pad: " + strings.Repeat("a", 17000) + "\r\n\r\n",
		"body over cap":     "POST /compute/x HTTP/1.1\r\n" + hc + fmt.Sprintf("Content-Length: %d\r\n\r\n", maxBodyBytes+1),
		"tunnel port":       "GET /api/moonlight/tunnel?port=22&transport=tcp HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel transport":  "GET /api/moonlight/tunnel?port=47998&transport=tcp HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel zero pad":   "GET /api/moonlight/tunnel?port=047984&transport=tcp HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel extra":      "GET /api/moonlight/tunnel?port=47984&transport=tcp&x=1 HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel dup port":   "GET /api/moonlight/tunnel?port=47984&port=47989 HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel no query":   "GET /api/moonlight/tunnel HTTP/1.1\r\n" + h + tunnelHeaders + "\r\n",
		"tunnel no upgrade": "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + hc + "\r\n",
		"tunnel ws proto":   "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + h + "Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
		"tunnel post":       "POST /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + h + tunnelHeaders + "Content-Length: 0\r\n\r\n",
		"ws version":        "GET /websockify HTTP/1.1\r\n" + h + strings.Replace(wsHeaders(rfcKey), "13", "12", 1) + "\r\n",
		"ws short key":      "GET /websockify HTTP/1.1\r\n" + h + wsHeaders("AAAA") + "\r\n",
		"ws no key":         "GET /websockify HTTP/1.1\r\n" + h + "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\r\n",
		"ws query":          "GET /websockify?token=x HTTP/1.1\r\n" + h + wsHeaders(rfcKey) + "\r\n",
		"ws no connection":  "GET /websockify HTTP/1.1\r\n" + h + "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + rfcKey + "\r\n\r\n",
		"ws body":           "GET /websockify HTTP/1.1\r\n" + h + wsHeaders(rfcKey) + "Content-Length: 1\r\n\r\nx",
	}
	for name, req := range cases {
		if code, _ := rawRoundTrip(t, addr, req); code < 400 {
			t.Errorf("%s: got status %d", name, code)
		}
	}
	if n := inner.hits.Load(); n != 0 {
		t.Fatalf("inner gateway contacted %d times", n)
	}
}

func TestProxyBodyCapExact(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	resp, err := testClient().Post("http://"+addr+"/compute/upload", "application/octet-stream", bytes.NewReader(make([]byte, maxBodyBytes)))
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if _, _, _, n := inner.seen(); resp.StatusCode != 200 || n != maxBodyBytes {
		t.Fatalf("status %d, upstream received %d bytes", resp.StatusCode, n)
	}
	code, _ := rawRoundTrip(t, addr, fmt.Sprintf("POST /compute/upload HTTP/1.1\r\nHost: %s\r\nContent-Length: %d\r\n\r\n", addr, maxBodyBytes+1))
	if code != 413 {
		t.Fatalf("over-cap body: %d", code)
	}
	if inner.hits.Load() != 1 {
		t.Fatal("over-cap body reached inner gateway")
	}
}

func TestProxyNeverFollowsRedirects(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	resp, err := testClient().Get("http://" + addr + "/compute/redirect")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 302 || resp.Header.Get("Location") != "/vnc.html" || inner.hits.Load() != 1 {
		t.Fatalf("relative redirect: %d %q hits=%d", resp.StatusCode, resp.Header.Get("Location"), inner.hits.Load())
	}
	resp, err = testClient().Get("http://" + addr + "/compute/absredirect")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 502 || resp.Header.Get("Location") != "" || inner.hits.Load() != 2 {
		t.Fatalf("absolute redirect: %d %q", resp.StatusCode, resp.Header.Get("Location"))
	}
}

func TestProxyClientCancellationReachesUpstream(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	ctx, cancel := context.WithCancel(context.Background())
	req, _ := http.NewRequestWithContext(ctx, "GET", "http://"+addr+"/compute/slow", nil)
	done := make(chan struct{})
	go func() {
		if resp, err := testClient().Do(req); err == nil {
			resp.Body.Close()
		}
		close(done)
	}()
	eventually(t, "upstream request", func() bool { return inner.hits.Load() == 1 })
	cancel()
	waitEvent(t, inner, "cancelled")
	<-done
}

func TestProxyUpstreamDownIsGeneric(t *testing.T) {
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	dead := ln.Addr().String()
	ln.Close()
	_, addr := startGateway(t, testConfig(dead, nil))
	code, body := rawRoundTrip(t, addr, "GET /vnc.html HTTP/1.1\r\nHost: "+addr+"\r\nConnection: close\r\n\r\n")
	if code != 502 || body != "{\"error\":\"bad gateway\"}\n" {
		t.Fatalf("got %d %q", code, body)
	}
}

// ---- upgrades over real TCP ----

func TestWebSocketUpgrade(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	c, br, resp := dialUpgrade(addr, "/websockify", wsHeaders(rfcKey)+"Authorization: Basic b3duZXI6c2VjcmV0\r\nCookie: a=b\r\n")
	if c == nil {
		t.Fatalf("upgrade failed: %d", status(resp))
	}
	defer c.Close()
	if resp.Header.Get("Sec-WebSocket-Accept") != rfcAccept || !strings.EqualFold(resp.Header.Get("Upgrade"), "websocket") {
		t.Fatalf("bad 101 headers: %v", resp.Header)
	}
	io.WriteString(c, "hello")
	got := make([]byte, 5)
	if _, err := io.ReadFull(br, got); err != nil || string(got) != "hello" {
		t.Fatalf("echo %q %v", got, err)
	}
	hdr, _, uri, _ := inner.seen()
	if uri != "/websockify" || hdr.Get("Sec-WebSocket-Key") != rfcKey || hdr.Get("Authorization") != "" || hdr.Get("Cookie") != "" {
		t.Fatalf("upstream saw uri %q headers %v", uri, hdr)
	}

	// An upstream that answers with the wrong accept value is refused.
	if c2, _, resp2 := dialUpgrade(addr, "/websockify", wsHeaders("AAAAAAAAAAAAAAAAAAAAAA==")); c2 != nil || status(resp2) != 502 {
		t.Fatalf("wrong upstream accept: %d", status(resp2))
	}
}

func TestTunnelHalfCloseAndCanonicalQuery(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	c, br, resp := dialUpgrade(addr, "/api/moonlight/tunnel?transport=udp&port=47998", tunnelHeaders)
	if c == nil {
		t.Fatalf("tunnel refused: %d", status(resp))
	}
	defer c.Close()
	if resp.Header.Get("Upgrade") != "kvm-box-tunnel" {
		t.Fatalf("upgrade header %q", resp.Header.Get("Upgrade"))
	}
	if _, _, uri, _ := inner.seen(); uri != "/api/moonlight/tunnel?port=47998&transport=udp" {
		t.Fatalf("upstream uri %q", uri)
	}
	io.WriteString(c, "ping")
	got := make([]byte, 4)
	if _, err := io.ReadFull(br, got); err != nil || string(got) != "ping" {
		t.Fatalf("echo %q %v", got, err)
	}
	// Half-close: upstream must see EOF and still be able to answer.
	if err := c.(*net.TCPConn).CloseWrite(); err != nil {
		t.Fatal(err)
	}
	rest, err := io.ReadAll(br)
	if err != nil || string(rest) != "EOF-SEEN" {
		t.Fatalf("after half-close got %q %v", rest, err)
	}
	waitEvent(t, inner, "eof")
}

func TestSimultaneousTunnels(t *testing.T) {
	inner := startInner(t)
	_, addr := startGateway(t, testConfig(inner.addr(), nil))
	targets := []string{"port=47984&transport=tcp", "port=47989&transport=tcp", "port=48010&transport=tcp",
		"port=47998&transport=udp", "port=47999&transport=udp", "port=48000&transport=udp"}
	type stream struct {
		c  net.Conn
		br *bufio.Reader
	}
	var streams []stream
	for _, q := range targets {
		c, br, resp := dialUpgrade(addr, "/api/moonlight/tunnel?"+q, tunnelHeaders)
		if c == nil {
			t.Fatalf("%s refused: %d", q, status(resp))
		}
		defer c.Close()
		streams = append(streams, stream{c, br})
	}
	var wg sync.WaitGroup
	for i, s := range streams {
		wg.Add(1)
		go func(i int, s stream) {
			defer wg.Done()
			payload := bytes.Repeat([]byte{byte('a' + i)}, 64<<10)
			go func() {
				s.c.Write(payload)
				s.c.(*net.TCPConn).CloseWrite()
			}()
			got, err := io.ReadAll(s.br)
			if err != nil || !bytes.Equal(got, append(payload, "EOF-SEEN"...)) {
				t.Errorf("stream %d: %d bytes, err %v", i, len(got), err)
			}
		}(i, s)
	}
	wg.Wait()
}

func TestTunnelClientCloseTearsDownUpstream(t *testing.T) {
	inner := startInner(t)
	g, addr := startGateway(t, testConfig(inner.addr(), nil))
	c, _, resp := dialUpgrade(addr, "/api/moonlight/tunnel?port=47984&transport=tcp", tunnelHeaders)
	if c == nil {
		t.Fatalf("tunnel refused: %d", status(resp))
	}
	c.Close()
	waitEvent(t, inner, "eof", "closed")
	eventually(t, "slot release", func() bool { return len(g.slots) == 0 })
}

func TestGatewayCloseCancelsTunnels(t *testing.T) {
	inner := startInner(t)
	g, addr := startGateway(t, testConfig(inner.addr(), nil))
	c, br, resp := dialUpgrade(addr, "/api/moonlight/tunnel?port=47989&transport=tcp", tunnelHeaders)
	if c == nil {
		t.Fatalf("tunnel refused: %d", status(resp))
	}
	defer c.Close()
	g.Close()
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	if _, err := br.ReadByte(); err == nil || isTimeout(err) {
		t.Fatalf("tunnel survived gateway close: %v", err)
	}
	waitEvent(t, inner, "eof", "closed")
	eventually(t, "slot release", func() bool { return len(g.slots) == 0 })
}

func TestTunnelIdleTimeout(t *testing.T) {
	inner := startInner(t)
	cfg := testConfig(inner.addr(), nil)
	cfg.tunnelIdle = 200 * time.Millisecond
	_, addr := startGateway(t, cfg)
	c, br, resp := dialUpgrade(addr, "/api/moonlight/tunnel?port=48010&transport=tcp", tunnelHeaders)
	if c == nil {
		t.Fatalf("tunnel refused: %d", status(resp))
	}
	defer c.Close()
	start := time.Now()
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	if _, err := br.ReadByte(); err == nil || isTimeout(err) {
		t.Fatalf("idle tunnel not closed by gateway: %v", err)
	}
	if d := time.Since(start); d > 2*time.Second {
		t.Fatalf("idle close took %v", d)
	}
}

func TestActiveConnectionLimit(t *testing.T) {
	inner := startInner(t)
	g, addr := startGateway(t, testConfig(inner.addr(), nil))
	var conns []net.Conn
	defer func() {
		for _, c := range conns {
			c.Close()
		}
	}()
	for i := 0; i < g.cfg.maxActive; i++ {
		c, _, resp := dialUpgrade(addr, "/api/moonlight/tunnel?port=47984&transport=tcp", tunnelHeaders)
		if c == nil {
			t.Fatalf("tunnel %d refused: %d", i, status(resp))
		}
		conns = append(conns, c)
	}
	hits := inner.hits.Load()
	if code, _ := rawRoundTrip(t, addr, "GET /vnc.html HTTP/1.1\r\nHost: "+addr+"\r\nConnection: close\r\n\r\n"); code != 0 && code != 503 {
		t.Fatalf("connection over limit served: %d", code)
	}
	if inner.hits.Load() != hits {
		t.Fatal("connection over limit reached inner gateway")
	}
	conns[0].Close()
	conns = conns[1:]
	eventually(t, "freed slot", func() bool {
		c, _, _ := dialUpgrade(addr, "/api/moonlight/tunnel?port=47984&transport=tcp", tunnelHeaders)
		if c == nil {
			return false
		}
		conns = append(conns, c)
		return true
	})
}
