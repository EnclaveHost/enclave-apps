//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

const (
	hostLine       = "Host: 127.0.0.1:18080\r\n"
	testWSKey      = "dGhlIHNhbXBsZSBub25jZQ=="
	testWSAccept   = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
	testCredential = "s3cret-Cred_42"
)

func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

// tempBase returns a short, symlink-free, 0755 temporary directory.
func tempBase(t *testing.T) string {
	t.Helper()
	base, err := os.MkdirTemp("", "ig")
	must(t, err)
	t.Cleanup(func() { os.RemoveAll(base) })
	base, err = filepath.EvalSymlinks(base)
	must(t, err)
	must(t, os.Chmod(base, 0o755))
	return base
}

func closedAddr(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	addr := ln.Addr().String()
	ln.Close()
	return addr
}

// testConfig starts from production values and redirects every endpoint to
// a refused address or a missing path, so a test only reaches what it sets up.
func testConfig(t *testing.T) config {
	t.Helper()
	cfg := productionConfig()
	base := tempBase(t)
	refused := closedAddr(t)
	cfg.staticAnchor = base
	cfg.novncRoot = filepath.Join(base, "novnc")
	cfg.nativeDir = filepath.Join(base, "native")
	cfg.staticUID = uint32(os.Getuid())
	cfg.requireReadOnly = false
	cfg.computeSocket = filepath.Join(base, "absent", "compute.sock")
	cfg.wsAddr = refused
	cfg.tunnelTarget = func(string, string) string { return refused }
	cfg.sunshineAddr = refused
	cfg.credPath = filepath.Join(base, "absent", "sunshine-password")
	cfg.handshake = 2 * time.Second
	return cfg
}

func startGateway(t *testing.T, cfg config) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	g := newGateway(cfg)
	go g.Serve(ln)
	t.Cleanup(func() { g.Close() })
	return ln.Addr().String()
}

func exchange(t *testing.T, addr, raw string) (*http.Response, string) {
	t.Helper()
	c, err := net.Dial("tcp", addr)
	must(t, err)
	defer c.Close()
	c.SetDeadline(time.Now().Add(10 * time.Second))
	_, err = io.WriteString(c, raw)
	must(t, err)
	method, _, _ := strings.Cut(raw, " ")
	resp, err := http.ReadResponse(bufio.NewReader(c), &http.Request{Method: method})
	if err != nil {
		t.Fatalf("read response: %v", err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	return resp, string(body)
}

func get(target string, hdrs ...string) string {
	return "GET " + target + " HTTP/1.1\r\n" + hostLine + strings.Join(hdrs, "") + "\r\n"
}

func head(target string) string {
	return "HEAD " + target + " HTTP/1.1\r\n" + hostLine + "\r\n"
}

func post(target, ctype, body string, hdrs ...string) string {
	return fmt.Sprintf("POST %s HTTP/1.1\r\n%sContent-Type: %s\r\nContent-Length: %d\r\n%s\r\n%s",
		target, hostLine, ctype, len(body), strings.Join(hdrs, ""), body)
}

func expectStatus(t *testing.T, resp *http.Response, code int) {
	t.Helper()
	if resp.StatusCode != code {
		t.Fatalf("status %d, want %d", resp.StatusCode, code)
	}
}

func expectSecurityHeaders(t *testing.T, resp *http.Response) {
	t.Helper()
	want := map[string]string{
		"X-Content-Type-Options":  "nosniff",
		"Cache-Control":           "no-store",
		"Content-Security-Policy": "frame-ancestors 'self'",
		"X-Frame-Options":         "SAMEORIGIN",
	}
	for k, v := range want {
		if got := resp.Header.Get(k); got != v {
			t.Errorf("%s = %q, want %q", k, got, v)
		}
	}
}

func TestStartupCheck(t *testing.T) {
	none := func(string) (string, bool) { return "", false }
	if err := startupCheck(1003, 1003, none, []string{"inner"}); err != nil {
		t.Fatalf("valid startup refused: %v", err)
	}
	if startupCheck(1003, 1003, none, []string{"inner", "--listen=0.0.0.0:80"}) == nil {
		t.Error("arguments accepted")
	}
	if startupCheck(1000, 1003, none, []string{"inner"}) == nil || startupCheck(1003, 0, none, []string{"inner"}) == nil {
		t.Error("wrong uid/euid accepted")
	}
	for _, k := range []string{"KVMBOX_OWNER_PASSWORD", "KVMBOX_ACCESS_SHA256"} {
		env := func(name string) (string, bool) { return "", name == k }
		if startupCheck(1003, 1003, env, []string{"inner"}) == nil {
			t.Errorf("%s accepted", k)
		}
	}
}

func TestProductionEndpointsAreFixed(t *testing.T) {
	cfg := productionConfig()
	if cfg.novncRoot != "/usr/share/kvm-box/native-ui/novnc" || cfg.nativeDir != "/usr/share/kvm-box/native-ui" ||
		cfg.staticAnchor != "/" || cfg.staticUID != 0 || !cfg.requireReadOnly {
		t.Error("static configuration changed")
	}
	if cfg.computeSocket != "/run/shield/compute.sock" || cfg.computePeerUID != 1001 || cfg.computeDirUID != 1001 ||
		cfg.computeSockUID != 1001 || cfg.computeGID != 1000 {
		t.Error("compute configuration changed")
	}
	if cfg.wsAddr != "127.0.0.1:6080" || cfg.sunshineAddr != "127.0.0.1:47990" ||
		cfg.credPath != "/run/native-ui/sunshine-password" || cfg.credUID != 1003 || cfg.credGID != 1003 {
		t.Error("upstream configuration changed")
	}
	if got := cfg.tunnelTarget("udp", "47998"); got != "127.0.0.1:47998" {
		t.Errorf("tunnel target %q", got)
	}
	if wsAccept(testWSKey) != testWSAccept {
		t.Error("RFC 6455 accept vector mismatch")
	}
}

func TestCommonRequestRejections(t *testing.T) {
	addr := startGateway(t, testConfig(t))
	cases := []struct {
		name, raw string
		code      int
	}{
		{"wrong host", "GET /vnc.html HTTP/1.1\r\nHost: localhost:18080\r\n\r\n", 400},
		{"http/1.0", "GET /vnc.html HTTP/1.0\r\n" + hostLine + "\r\n", 400},
		{"absolute uri", "GET http://127.0.0.1:18080/vnc.html HTTP/1.1\r\n" + hostLine + "\r\n", 400},
		{"options star", "OPTIONS * HTTP/1.1\r\n" + hostLine + "\r\n", 405},
		{"put", "PUT /vnc.html HTTP/1.1\r\n" + hostLine + "Content-Length: 0\r\n\r\n", 405},
		{"unknown route", get("/etc/passwd"), 404},
		{"generic compute proxy", get("/compute/v1/models"), 404},
		{"compute without slash", get("/compute"), 404},
		{"encoded slash", get("/app/a%2Fb.js"), 400},
		{"encoded dots", get("/app/%2e%2e/vnc.html"), 400},
		{"dot segment", get("/app/../vnc.html"), 400},
		{"double slash", get("//vnc.html"), 400},
		{"backslash", get("/app/..\\vnc.html"), 400},
		{"authorization", get("/vnc.html", "Authorization: Basic b3duZXI6eA==\r\n"), 400},
		{"cookie", get("/vnc.html", "Cookie: session=1\r\n"), 400},
		{"proxy authorization", get("/vnc.html", "Proxy-Authorization: Basic eDp4\r\n"), 400},
		{"get with body", "GET /vnc.html HTTP/1.1\r\n" + hostLine + "Content-Length: 2\r\n\r\nhi", 400},
		{"chunked", "POST /compute/native/compute HTTP/1.1\r\n" + hostLine +
			"Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n", 400},
		{"content encoding", post("/compute/native/compute", "application/json", `{"op":"llama"}`, "Content-Encoding: gzip\r\n"), 400},
		{"duplicate content type", post("/compute/native/compute", "application/json", `{"op":"llama"}`, "Content-Type: application/json\r\n"), 400},
		{"upgrade on static", get("/vnc.html", "Connection: Upgrade\r\nUpgrade: websocket\r\n"), 400},
		{"query on compute api", get("/compute/api/native?x=1"), 400},
		{"bad query byte", get("/vnc.html?a=<b>"), 400},
		{"head compute api", head("/compute/api/native"), 405},
		{"get pair", get("/api/moonlight/pair"), 405},
		{"post static", post("/vnc.html", "application/json", `{}`), 405},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			resp, _ := exchange(t, addr, tc.raw)
			expectStatus(t, resp, tc.code)
		})
	}
	resp, body := exchange(t, addr, get("/nope"))
	expectSecurityHeaders(t, resp)
	if resp.Header.Get("Content-Type") != "application/json" || body != `{"error":"not found"}` {
		t.Errorf("error response %q %q", resp.Header.Get("Content-Type"), body)
	}
}

// Static assets.

func writeStaticTree(t *testing.T, cfg config) {
	t.Helper()
	files := map[string]string{
		"novnc/vnc.html":                        "<!doctype html><title>vnc</title>",
		"novnc/app/ui.js":                       "export const ui = 1;",
		"novnc/app/styles/base.css":             "body{}",
		"novnc/app/images/icon.svg":             "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
		"novnc/app/locale/de.json":              "{}",
		"novnc/app/run.sh":                      "#!/bin/sh\n",
		"novnc/app/.hidden.js":                  "hidden",
		"novnc/app/UPPER.JS":                    "upper",
		"novnc/core/rfb.js":                     "export default class RFB {}",
		"novnc/vendor/pako/lib/zlib/inflate.js": "inflate",
		"native/native.html":                    "<!doctype html><title>native</title>",
		"outside/secret.js":                     "secret",
	}
	for rel, data := range files {
		p := filepath.Join(cfg.staticAnchor, rel)
		must(t, os.MkdirAll(filepath.Dir(p), 0o755))
		must(t, os.WriteFile(p, []byte(data), 0o644))
		must(t, os.Chmod(p, 0o644))
	}
	app := filepath.Join(cfg.novncRoot, "app")
	must(t, os.Symlink("../../outside/secret.js", filepath.Join(app, "escape.js")))
	must(t, os.Symlink("ui.js", filepath.Join(app, "alias.js")))
	must(t, os.Symlink("../core", filepath.Join(app, "sub")))
	must(t, filepath.Walk(cfg.staticAnchor, func(p string, fi os.FileInfo, err error) error {
		if err == nil && fi.IsDir() {
			return os.Chmod(p, 0o755)
		}
		return err
	}))
}

func TestStaticServing(t *testing.T) {
	cfg := testConfig(t)
	writeStaticTree(t, cfg)
	addr := startGateway(t, cfg)

	ok := []struct{ target, ctype, body string }{
		{"/vnc.html", "text/html; charset=utf-8", "<!doctype html><title>vnc</title>"},
		{"/vnc.html?autoconnect=true&path=websockify", "text/html; charset=utf-8", "<!doctype html><title>vnc</title>"},
		{"/app/ui.js", "text/javascript; charset=utf-8", "export const ui = 1;"},
		{"/app/styles/base.css", "text/css; charset=utf-8", "body{}"},
		{"/app/images/icon.svg", "image/svg+xml", "<svg xmlns=\"http://www.w3.org/2000/svg\"/>"},
		{"/app/locale/de.json", "application/json", "{}"},
		{"/core/rfb.js", "text/javascript; charset=utf-8", "export default class RFB {}"},
		{"/vendor/pako/lib/zlib/inflate.js", "text/javascript; charset=utf-8", "inflate"},
		{"/compute/", "text/html; charset=utf-8", "<!doctype html><title>native</title>"},
		{"/compute/native/", "text/html; charset=utf-8", "<!doctype html><title>native</title>"},
	}
	for _, tc := range ok {
		t.Run("ok "+tc.target, func(t *testing.T) {
			resp, body := exchange(t, addr, get(tc.target))
			expectStatus(t, resp, 200)
			expectSecurityHeaders(t, resp)
			if resp.Header.Get("Content-Type") != tc.ctype || body != tc.body {
				t.Fatalf("got %q %q", resp.Header.Get("Content-Type"), body)
			}
		})
	}

	resp, body := exchange(t, addr, head("/vnc.html"))
	expectStatus(t, resp, 200)
	if body != "" || resp.ContentLength != int64(len("<!doctype html><title>vnc</title>")) {
		t.Errorf("HEAD body %q length %d", body, resp.ContentLength)
	}

	for _, target := range []string{
		"/app/run.sh",         // extension not allowlisted
		"/app/.hidden.js",     // hidden name
		"/app/UPPER.JS",       // extension aliases are not normalised
		"/app/images/",        // no directory listings
		"/app/images",         // directory without slash
		"/app/escape.js",      // symlink leaving the tree
		"/app/alias.js",       // symlink inside the tree
		"/app/sub/rfb.js",     // symlinked directory
		"/app/missing.js",     // absent
		"/compute/native",     // only the two fixed page routes
		"/compute/index.html", // not a route
		"/native.html",        // native page is not a static root
		"/outside/secret.js",  // not a route
	} {
		t.Run("404 "+target, func(t *testing.T) {
			resp, body := exchange(t, addr, get(target))
			expectStatus(t, resp, 404)
			if strings.Contains(body, "secret") || strings.Contains(body, "export") {
				t.Fatalf("asset content leaked: %q", body)
			}
		})
	}
}

func TestStaticIntegrityRefusal(t *testing.T) {
	cases := []struct {
		name   string
		mutate func(t *testing.T, cfg *config)
		target string
	}{
		{"foreign owner", func(t *testing.T, cfg *config) { cfg.staticUID = uint32(os.Getuid()) + 1 }, "/vnc.html"},
		{"foreign owner native page", func(t *testing.T, cfg *config) { cfg.staticUID = uint32(os.Getuid()) + 1 }, "/compute/"},
		{"group writable file", func(t *testing.T, cfg *config) {
			must(t, os.Chmod(filepath.Join(cfg.novncRoot, "app", "ui.js"), 0o664))
		}, "/app/ui.js"},
		{"world writable directory", func(t *testing.T, cfg *config) {
			must(t, os.Chmod(filepath.Join(cfg.novncRoot, "core"), 0o757))
		}, "/core/rfb.js"},
		{"writable tree root", func(t *testing.T, cfg *config) { must(t, os.Chmod(cfg.novncRoot, 0o775)) }, "/vnc.html"},
		{"writable anchor", func(t *testing.T, cfg *config) { must(t, os.Chmod(cfg.staticAnchor, 0o777)) }, "/vnc.html"},
		{"symlinked tree root", func(t *testing.T, cfg *config) {
			link := filepath.Join(cfg.staticAnchor, "novnc-link")
			must(t, os.Symlink("novnc", link))
			cfg.novncRoot = link
		}, "/vnc.html"},
		{"writable filesystem", func(t *testing.T, cfg *config) { cfg.requireReadOnly = true }, "/vnc.html"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := testConfig(t)
			writeStaticTree(t, cfg)
			tc.mutate(t, &cfg)
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, get(tc.target))
			expectStatus(t, resp, 503)
			if strings.Contains(body, "doctype") || strings.Contains(body, "export") {
				t.Fatalf("asset served despite refusal: %q", body)
			}
		})
	}

	// A refusal is per file: an untouched sibling is still served.
	cfg := testConfig(t)
	writeStaticTree(t, cfg)
	must(t, os.Chmod(filepath.Join(cfg.novncRoot, "app", "ui.js"), 0o666))
	addr := startGateway(t, cfg)
	resp, _ := exchange(t, addr, get("/vnc.html"))
	expectStatus(t, resp, 200)
}

// Native compute.

type computeFixture struct {
	dir    string
	path   string
	conns  atomic.Int32
	frames chan []byte
}

func frame(s string) []byte {
	b := make([]byte, 4+len(s))
	binary.BigEndian.PutUint32(b, uint32(len(s)))
	copy(b[4:], s)
	return b
}

// startCompute runs a real Unix-socket service with production-shaped
// permissions (dir 0750, socket 0660) and points cfg at it.
func startCompute(t *testing.T, cfg *config, handle func(c net.Conn, req []byte)) *computeFixture {
	t.Helper()
	base := tempBase(t)
	dir := filepath.Join(base, "shield")
	must(t, os.Mkdir(dir, 0o750))
	must(t, os.Chmod(dir, 0o750))
	path := filepath.Join(dir, "compute.sock")
	ln, err := net.Listen("unix", path)
	must(t, err)
	t.Cleanup(func() { ln.Close() })
	must(t, os.Chmod(path, 0o660))
	var dst, sst syscall.Stat_t
	must(t, syscall.Lstat(dir, &dst))
	must(t, syscall.Lstat(path, &sst))
	if dst.Gid != sst.Gid {
		t.Skip("socket and directory groups differ on this filesystem")
	}
	cfg.computeSocket = path
	cfg.computeDirUID = dst.Uid
	cfg.computeSockUID = sst.Uid
	cfg.computeGID = dst.Gid
	cfg.computePeerUID = uint32(os.Getuid())
	f := &computeFixture{dir: dir, path: path, frames: make(chan []byte, 16)}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			f.conns.Add(1)
			go func(c net.Conn) {
				defer c.Close()
				c.SetReadDeadline(time.Now().Add(5 * time.Second))
				var hdr [4]byte
				if _, err := io.ReadFull(c, hdr[:]); err != nil {
					return
				}
				n := binary.BigEndian.Uint32(hdr[:])
				if n > 1<<20 {
					return
				}
				req := make([]byte, n)
				if _, err := io.ReadFull(c, req); err != nil {
					return
				}
				f.frames <- req
				c.SetReadDeadline(time.Time{})
				handle(c, req)
			}(c)
		}
	}()
	return f
}

func reply(b []byte) func(net.Conn, []byte) {
	return func(c net.Conn, _ []byte) { c.Write(b) }
}

func TestComputeStatusRoundTrip(t *testing.T) {
	cfg := testConfig(t)
	status := `{"ok":true,"schema":"shield-guest-status/1","state":"ready","engines":["llama","sd"]}`
	f := startCompute(t, &cfg, reply(frame(status)))
	addr := startGateway(t, cfg)
	resp, body := exchange(t, addr, get("/compute/api/native"))
	expectStatus(t, resp, 200)
	expectSecurityHeaders(t, resp)
	if resp.Header.Get("Content-Type") != "application/json" || body != status {
		t.Fatalf("got %q %q", resp.Header.Get("Content-Type"), body)
	}
	if got := string(<-f.frames); got != `{"op":"status"}` {
		t.Fatalf("service received %q", got)
	}
}

func TestComputeWorkForwardsBodyUnchanged(t *testing.T) {
	cfg := testConfig(t)
	result := `{"ok":true,"mode":"cpu","text":"<script>alert(1)</script>"}`
	f := startCompute(t, &cfg, reply(frame(result)))
	addr := startGateway(t, cfg)
	body := `{ "prompt" : "hi <b>é", "op":"llama","seed":42,"mode":"cpu","arithmetic":"native-model-v1" }`
	resp, got := exchange(t, addr, post("/compute/native/compute", "application/json", body))
	expectStatus(t, resp, 200)
	if resp.Header.Get("Content-Type") != "application/json" || resp.Header.Get("X-Content-Type-Options") != "nosniff" || got != result {
		t.Fatalf("got %q %q", resp.Header.Get("Content-Type"), got)
	}
	if sent := string(<-f.frames); sent != body {
		t.Fatalf("service received %q, want unchanged %q", sent, body)
	}
}

func TestComputeWorkRejectsBeforeConnecting(t *testing.T) {
	cfg := testConfig(t)
	f := startCompute(t, &cfg, reply(frame(`{"ok":true}`)))
	addr := startGateway(t, cfg)
	const target = "/compute/native/compute"
	cases := []struct {
		name, raw string
		code      int
	}{
		{"status op", post(target, "application/json", `{"op":"status"}`), 400},
		{"graph op", post(target, "application/json", `{"op":"graph","graph":{}}`), 400},
		{"q8 op", post(target, "application/json", `{"op":"q8_0_matmul"}`), 400},
		{"missing op", post(target, "application/json", `{"prompt":"x"}`), 400},
		{"duplicate op", post(target, "application/json", `{"op":"sd","op":"llama"}`), 400},
		{"escaped duplicate op", post(target, "application/json", `{"op":"llama","op":"graph"}`), 400},
		{"nested duplicate", post(target, "application/json", `{"op":"llama","x":{"a":1,"a":2}}`), 400},
		{"array", post(target, "application/json", `[{"op":"llama"}]`), 400},
		{"second value", post(target, "application/json", `{"op":"llama"} {}`), 400},
		{"trailing garbage", post(target, "application/json", `{"op":"llama"}x`), 400},
		{"numeric op", post(target, "application/json", `{"op":1}`), 400},
		{"invalid utf-8", post(target, "application/json", "{\"op\":\"llama\",\"prompt\":\"\xff\"}"), 400},
		{"empty body", post(target, "application/json", ""), 400},
		{"text/plain", post(target, "text/plain", `{"op":"llama"}`), 415},
		{"latin1 charset", post(target, "application/json; charset=latin1", `{"op":"llama"}`), 415},
		{"too large", post(target, "application/json", `{"op":"llama","prompt":"`+strings.Repeat("a", 64<<10)+`"}`), 413},
		{"query", post(target+"?engine=llama", "application/json", `{"op":"llama"}`), 400},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			resp, _ := exchange(t, addr, tc.raw)
			expectStatus(t, resp, tc.code)
		})
	}
	if n := f.conns.Load(); n != 0 {
		t.Fatalf("compute socket contacted %d times for rejected requests", n)
	}
}

func TestComputeSocketAndPeerChecks(t *testing.T) {
	cases := []struct {
		name     string
		mutate   func(t *testing.T, cfg *config, f *computeFixture)
		connects bool
	}{
		{"peer uid not admitted", func(t *testing.T, cfg *config, f *computeFixture) {
			cfg.computePeerUID = uint32(os.Getuid()) + 1
		}, true},
		{"directory mode 0755", func(t *testing.T, cfg *config, f *computeFixture) { must(t, os.Chmod(f.dir, 0o755)) }, false},
		{"socket mode 0666", func(t *testing.T, cfg *config, f *computeFixture) { must(t, os.Chmod(f.path, 0o666)) }, false},
		{"wrong socket owner", func(t *testing.T, cfg *config, f *computeFixture) { cfg.computeSockUID++ }, false},
		{"wrong group", func(t *testing.T, cfg *config, f *computeFixture) { cfg.computeGID++ }, false},
		{"symlinked socket", func(t *testing.T, cfg *config, f *computeFixture) {
			link := filepath.Join(f.dir, "link.sock")
			must(t, os.Symlink(f.path, link))
			cfg.computeSocket = link
		}, false},
		{"symlinked directory", func(t *testing.T, cfg *config, f *computeFixture) {
			link := filepath.Join(filepath.Dir(f.dir), "shield-link")
			must(t, os.Symlink(f.dir, link))
			cfg.computeSocket = filepath.Join(link, "compute.sock")
		}, false},
		{"missing socket", func(t *testing.T, cfg *config, f *computeFixture) {
			cfg.computeSocket = filepath.Join(f.dir, "absent.sock")
		}, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := testConfig(t)
			f := startCompute(t, &cfg, reply(frame(`{"ok":true}`)))
			tc.mutate(t, &cfg, f)
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, get("/compute/api/native"))
			expectStatus(t, resp, 503)
			if body != `{"error":"service unavailable"}` {
				t.Fatalf("body %q", body)
			}
			if !tc.connects && f.conns.Load() != 0 {
				t.Fatal("connected despite path refusal")
			}
			select {
			case req := <-f.frames:
				t.Fatalf("request %q sent to refused service", req)
			case <-time.After(100 * time.Millisecond):
			}
		})
	}
}

func TestComputeMalformedResponses(t *testing.T) {
	oversize := []byte{0x01, 0x00, 0x00, 0x01} // 16 MiB + 1
	cases := []struct {
		name  string
		bytes []byte
		code  int
	}{
		{"garbage inside frame", frame(`{"ok":true} <html>`), 502},
		{"bytes after frame", append(frame(`{"ok":true}`), "<html>"...), 502},
		{"second frame", append(frame(`{"ok":true}`), frame(`{"ok":true}`)...), 502},
		{"json string", frame(`"<html>"`), 502},
		{"html", frame(`<html><script>x</script></html>`), 502},
		{"invalid json", frame(`{"ok":tru}`), 502},
		{"trailing whitespace", frame("{\"ok\":true}\n"), 502},
		{"zero length", []byte{0, 0, 0, 0}, 502},
		{"oversize length", oversize, 502},
		{"truncated body", append([]byte{0, 0, 0, 100}, `{"ok":true`...), 502},
		{"truncated header", []byte{0, 0}, 502},
		{"closed without reply", nil, 503},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := testConfig(t)
			startCompute(t, &cfg, reply(tc.bytes))
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, get("/compute/api/native"))
			expectStatus(t, resp, tc.code)
			if strings.Contains(body, "html") || strings.Contains(body, "ok") {
				t.Fatalf("backend bytes reflected: %q", body)
			}
		})
	}
}

// drip writes a large length header, then one body byte every 20ms, and
// reports the write error that ends it (nil if it was never cut off).
func drip(done chan<- error) func(net.Conn, []byte) {
	return func(c net.Conn, _ []byte) {
		c.Write([]byte{0, 0, 3, 232})
		for i := 0; i < 250; i++ {
			time.Sleep(20 * time.Millisecond)
			if _, err := c.Write([]byte{' '}); err != nil {
				done <- err
				return
			}
		}
		done <- nil
	}
}

func TestComputeAbsoluteDeadlineIgnoresDripFeed(t *testing.T) {
	cfg := testConfig(t)
	cfg.statusTimeout = 300 * time.Millisecond
	cut := make(chan error, 1)
	startCompute(t, &cfg, drip(cut))
	addr := startGateway(t, cfg)
	start := time.Now()
	resp, _ := exchange(t, addr, get("/compute/api/native"))
	expectStatus(t, resp, 503)
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("drip feed extended the deadline: %v", elapsed)
	}
	select {
	case err := <-cut:
		if err == nil {
			t.Fatal("service socket stayed open")
		}
	case <-time.After(3 * time.Second):
		t.Fatal("service socket not closed after deadline")
	}
}

func TestComputeCancellationClosesSocket(t *testing.T) {
	cfg := testConfig(t)
	cfg.statusTimeout = 20 * time.Second
	cut := make(chan error, 1)
	f := startCompute(t, &cfg, drip(cut))
	addr := startGateway(t, cfg)
	c, err := net.Dial("tcp", addr)
	must(t, err)
	_, err = io.WriteString(c, get("/compute/api/native"))
	must(t, err)
	select {
	case <-f.frames:
	case <-time.After(5 * time.Second):
		t.Fatal("request never reached the service")
	}
	closed := time.Now()
	c.Close()
	select {
	case err := <-cut:
		if err == nil {
			t.Fatal("service socket stayed open")
		}
		if d := time.Since(closed); d > 2*time.Second {
			t.Fatalf("socket closed %v after cancellation", d)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("service socket not closed on client cancellation")
	}
}

// WebSocket.

type wsUpstream struct {
	addr    string
	accepts atomic.Int32
	reqs    chan *http.Request
}

func startWSUpstream(t *testing.T, respond func(key string) string, after func(c net.Conn, br *bufio.Reader)) *wsUpstream {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	t.Cleanup(func() { ln.Close() })
	u := &wsUpstream{addr: ln.Addr().String(), reqs: make(chan *http.Request, 4)}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			u.accepts.Add(1)
			go func(c net.Conn) {
				defer c.Close()
				c.SetDeadline(time.Now().Add(10 * time.Second))
				br := bufio.NewReader(c)
				req, err := http.ReadRequest(br)
				if err != nil {
					return
				}
				u.reqs <- req
				io.WriteString(c, respond(req.Header.Get("Sec-Websocket-Key")))
				if after != nil {
					after(c, br)
				}
			}(c)
		}
	}()
	return u
}

func ws101(key string, extra string) string {
	return "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " +
		wsAccept(key) + "\r\n" + extra + "\r\n"
}

func wsRequest(target string, extra ...string) string {
	return "GET " + target + " HTTP/1.1\r\n" + hostLine + "Connection: Upgrade\r\nUpgrade: websocket\r\n" +
		"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + testWSKey + "\r\n" + strings.Join(extra, "") + "\r\n"
}

func TestWebSocketProxy(t *testing.T) {
	u := startWSUpstream(t,
		func(key string) string { return ws101(key, "Sec-WebSocket-Protocol: binary\r\n") + "UPEARLY" },
		func(c net.Conn, br *bufio.Reader) {
			early := make([]byte, len("CLIENTEARLY"))
			if _, err := io.ReadFull(br, early); err != nil || string(early) != "CLIENTEARLY" {
				return
			}
			io.Copy(c, br) // echo until the client half-closes
			io.WriteString(c, "BYE")
		})
	cfg := testConfig(t)
	cfg.wsAddr = u.addr
	addr := startGateway(t, cfg)

	c, err := net.Dial("tcp", addr)
	must(t, err)
	defer c.Close()
	c.SetDeadline(time.Now().Add(10 * time.Second))
	req := wsRequest("/websockify",
		"Sec-WebSocket-Protocol: binary\r\n",
		"Sec-WebSocket-Extensions: permessage-deflate\r\n",
		"Origin: https://evil.example\r\n",
		"X-Forwarded-For: 10.0.0.1\r\n",
		"Forwarded: for=10.0.0.1\r\n",
		"User-Agent: test\r\n")
	_, err = io.WriteString(c, req+"CLIENTEARLY")
	must(t, err)
	br := bufio.NewReader(c)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodGet})
	must(t, err)
	expectStatus(t, resp, 101)
	if resp.Header.Get("Sec-Websocket-Accept") != testWSAccept || resp.Header.Get("Sec-Websocket-Protocol") != "binary" {
		t.Fatalf("handshake headers %v", resp.Header)
	}
	early := make([]byte, 7)
	_, err = io.ReadFull(br, early)
	must(t, err)
	if string(early) != "UPEARLY" {
		t.Fatalf("upstream early bytes %q", early)
	}
	_, err = io.WriteString(c, "ping")
	must(t, err)
	echo := make([]byte, 4)
	_, err = io.ReadFull(br, echo)
	must(t, err)
	if string(echo) != "ping" {
		t.Fatalf("echo %q", echo)
	}
	must(t, c.(*net.TCPConn).CloseWrite())
	rest, err := io.ReadAll(br)
	must(t, err)
	if string(rest) != "BYE" {
		t.Fatalf("after half-close got %q", rest)
	}

	up := <-u.reqs
	if up.RequestURI != "/websockify" || up.Host != u.addr {
		t.Errorf("upstream target %q host %q", up.RequestURI, up.Host)
	}
	if up.Header.Get("Sec-Websocket-Key") != testWSKey || up.Header.Get("Sec-Websocket-Version") != "13" ||
		up.Header.Get("Sec-Websocket-Protocol") != "binary" {
		t.Errorf("handshake fields not forwarded: %v", up.Header)
	}
	for _, k := range []string{"Origin", "X-Forwarded-For", "Forwarded", "User-Agent", "Sec-Websocket-Extensions"} {
		if v := up.Header.Values(k); len(v) != 0 {
			t.Errorf("%s forwarded upstream: %q", k, v)
		}
	}
}

func TestWebSocketHandshakeRejections(t *testing.T) {
	u := startWSUpstream(t, func(key string) string { return ws101(key, "") }, nil)
	cfg := testConfig(t)
	cfg.wsAddr = u.addr
	addr := startGateway(t, cfg)
	base := func(version, key string, extra ...string) string {
		return "GET /websockify HTTP/1.1\r\n" + hostLine + "Connection: Upgrade\r\nUpgrade: websocket\r\n" +
			"Sec-WebSocket-Version: " + version + "\r\nSec-WebSocket-Key: " + key + "\r\n" + strings.Join(extra, "") + "\r\n"
	}
	cases := []struct {
		name, raw string
		code      int
	}{
		{"version 12", base("12", testWSKey), 400},
		{"short key", base("13", "dGVzdA=="), 400},
		{"non-base64 key", base("13", "!!!!!!!!!!!!!!!!!!!!!!!!"), 400},
		{"other subprotocol", base("13", testWSKey, "Sec-WebSocket-Protocol: chat\r\n"), 400},
		{"subprotocol list", base("13", testWSKey, "Sec-WebSocket-Protocol: binary, base64\r\n"), 400},
		{"duplicate key", base("13", testWSKey, "Sec-WebSocket-Key: "+testWSKey+"\r\n"), 400},
		{"query", wsRequest("/websockify?token=abc"), 400},
		{"plain get", get("/websockify"), 400},
		{"h2c upgrade", "GET /websockify HTTP/1.1\r\n" + hostLine + "Connection: Upgrade\r\nUpgrade: h2c\r\n" +
			"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + testWSKey + "\r\n\r\n", 400},
		{"extra connection token", "GET /websockify HTTP/1.1\r\n" + hostLine + "Connection: Upgrade, close\r\nUpgrade: websocket\r\n" +
			"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + testWSKey + "\r\n\r\n", 400},
		{"cookie", wsRequest("/websockify", "Cookie: a=b\r\n"), 400},
		{"post", post("/websockify", "application/json", "{}"), 405},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			resp, _ := exchange(t, addr, tc.raw)
			expectStatus(t, resp, tc.code)
		})
	}
	if n := u.accepts.Load(); n != 0 {
		t.Fatalf("upstream contacted %d times for invalid handshakes", n)
	}
}

func TestWebSocketUpstreamValidation(t *testing.T) {
	cases := []struct {
		name    string
		respond func(key string) string
		code    int
	}{
		{"wrong accept", func(string) string { return ws101("AAAAAAAAAAAAAAAAAAAAAA==", "") }, 502},
		{"not switching", func(string) string {
			return "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 14\r\n\r\n<html>x</html>"
		}, 502},
		{"redirect", func(string) string {
			return "HTTP/1.1 302 Found\r\nLocation: http://evil.example/\r\nContent-Length: 0\r\n\r\n"
		}, 502},
		{"unoffered subprotocol", func(key string) string { return ws101(key, "Sec-WebSocket-Protocol: binary\r\n") }, 502},
		{"unoffered extension", func(key string) string { return ws101(key, "Sec-WebSocket-Extensions: permessage-deflate\r\n") }, 502},
		{"wrong upgrade token", func(key string) string {
			return "HTTP/1.1 101 Switching Protocols\r\nUpgrade: h2c\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + wsAccept(key) + "\r\n\r\n"
		}, 502},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			u := startWSUpstream(t, tc.respond, nil)
			cfg := testConfig(t)
			cfg.wsAddr = u.addr
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, wsRequest("/websockify"))
			expectStatus(t, resp, tc.code)
			if strings.Contains(body, "html") || strings.Contains(body, "evil") {
				t.Fatalf("upstream bytes reflected: %q", body)
			}
		})
	}
	t.Run("upstream down", func(t *testing.T) {
		addr := startGateway(t, testConfig(t))
		resp, _ := exchange(t, addr, wsRequest("/websockify"))
		expectStatus(t, resp, 503)
	})
}

// Moonlight tunnel.

func tunnelRequest(query string, extra ...string) string {
	return "GET /api/moonlight/tunnel?" + query + " HTTP/1.1\r\n" + hostLine +
		"Connection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n" + strings.Join(extra, "") + "\r\n"
}

func openTunnel(t *testing.T, addr, query string, early []byte) (*net.TCPConn, *bufio.Reader) {
	t.Helper()
	c, err := net.Dial("tcp", addr)
	must(t, err)
	t.Cleanup(func() { c.Close() })
	c.SetDeadline(time.Now().Add(10 * time.Second))
	_, err = c.Write(append([]byte(tunnelRequest(query)), early...))
	must(t, err)
	br := bufio.NewReader(c)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodGet})
	must(t, err)
	expectStatus(t, resp, 101)
	if resp.Header.Get("Upgrade") != "kvm-box-tunnel" {
		t.Fatalf("upgrade %q", resp.Header.Get("Upgrade"))
	}
	return c.(*net.TCPConn), br
}

func TestTunnelTCPHalfClose(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	defer ln.Close()
	received := make(chan string, 1)
	go func() {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		c.SetDeadline(time.Now().Add(10 * time.Second))
		data, _ := io.ReadAll(c) // returns only once the client's EOF is propagated
		received <- string(data)
		io.WriteString(c, "world")
	}()
	cfg := testConfig(t)
	refused := closedAddr(t)
	cfg.tunnelTarget = func(transport, port string) string {
		if transport == "tcp" && port == "47989" {
			return ln.Addr().String()
		}
		return refused
	}
	addr := startGateway(t, cfg)
	// Reversed field order, with early payload in the same segment.
	c, br := openTunnel(t, addr, "transport=tcp&port=47989", []byte("hel"))
	_, err = c.Write([]byte("lo"))
	must(t, err)
	must(t, c.CloseWrite())
	select {
	case got := <-received:
		if got != "hello" {
			t.Fatalf("upstream received %q", got)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("client half-close not propagated")
	}
	rest, err := io.ReadAll(br)
	must(t, err)
	if string(rest) != "world" {
		t.Fatalf("client received %q", rest)
	}
}

func TestTunnelQueryAndHeaderRejections(t *testing.T) {
	var dials atomic.Int32
	cfg := testConfig(t)
	refused := closedAddr(t)
	cfg.tunnelTarget = func(string, string) string { dials.Add(1); return refused }
	addr := startGateway(t, cfg)
	queries := []string{
		"port=47990&transport=tcp", // Sunshine admin API
		"port=047984&transport=tcp",
		"port=+47984&transport=tcp",
		"port=%34%37%39%38%34&transport=tcp",
		"port=47984&transport=%74cp",
		"port=47984&transport=TCP",
		"port=47998&transport=tcp",
		"port=47984&transport=udp",
		"port=47984&transport=tcp&port=47984",
		"port=47984&port=47989",
		"port=47984&transport=tcp&x=1",
		"port=47984&transport=tcp&",
		"port=47984",
		"port=&transport=tcp",
		"port=47984;transport=tcp",
		"",
	}
	for _, q := range queries {
		t.Run("query "+q, func(t *testing.T) {
			resp, _ := exchange(t, addr, tunnelRequest(q))
			expectStatus(t, resp, 400)
		})
	}
	headerCases := []struct{ name, raw string }{
		{"no query", "GET /api/moonlight/tunnel HTTP/1.1\r\n" + hostLine + "Connection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n\r\n"},
		{"upgrade case", "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + hostLine +
			"Connection: Upgrade\r\nUpgrade: KVM-Box-Tunnel\r\n\r\n"},
		{"websocket upgrade", "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + hostLine +
			"Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n"},
		{"missing connection", "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + hostLine +
			"Upgrade: kvm-box-tunnel\r\n\r\n"},
		{"duplicate upgrade", tunnelRequest("port=47984&transport=tcp", "Upgrade: kvm-box-tunnel\r\n")},
		{"body", "GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\n" + hostLine +
			"Connection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\nContent-Length: 1\r\n\r\nx"},
	}
	for _, tc := range headerCases {
		t.Run(tc.name, func(t *testing.T) {
			resp, _ := exchange(t, addr, tc.raw)
			expectStatus(t, resp, 400)
		})
	}
	if n := dials.Load(); n != 0 {
		t.Fatalf("dialled %d times for rejected tunnels", n)
	}
	// A valid request reaches the dial step and maps a refused port to 503.
	resp, _ := exchange(t, addr, tunnelRequest("port=48010&transport=tcp"))
	expectStatus(t, resp, 503)
	if dials.Load() != 1 {
		t.Fatal("valid tunnel was not dialled")
	}
}

func udpFrame(b []byte) []byte {
	out := make([]byte, 2+len(b))
	binary.BigEndian.PutUint16(out, uint16(len(b)))
	copy(out[2:], b)
	return out
}

func expectClosed(t *testing.T, c net.Conn, br *bufio.Reader) {
	t.Helper()
	c.SetReadDeadline(time.Now().Add(3 * time.Second))
	_, err := br.ReadByte()
	if err == nil {
		t.Fatal("tunnel delivered data after a framing error")
	}
	if ne, ok := err.(net.Error); ok && ne.Timeout() {
		t.Fatal("tunnel stayed open after a framing error")
	}
}

func expectNoDatagram(t *testing.T, up *net.UDPConn) {
	t.Helper()
	buf := make([]byte, 70000)
	up.SetReadDeadline(time.Now().Add(300 * time.Millisecond))
	if n, _, err := up.ReadFromUDP(buf); err == nil {
		t.Fatalf("datagram of %d bytes leaked upstream", n)
	}
}

func TestTunnelUDPFraming(t *testing.T) {
	up, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	must(t, err)
	defer up.Close()
	cfg := testConfig(t)
	refused := closedAddr(t)
	cfg.tunnelTarget = func(transport, port string) string {
		if transport == "udp" && port == "47999" {
			return up.LocalAddr().String()
		}
		return refused
	}
	addr := startGateway(t, cfg)

	t.Run("boundaries both ways", func(t *testing.T) {
		big := bytes.Repeat([]byte{0xa5}, 60000)
		early := append(udpFrame([]byte("abc")), udpFrame([]byte("defgh"))...)
		c, br := openTunnel(t, addr, "port=47999&transport=udp", early)
		_, err := c.Write(udpFrame(big))
		must(t, err)
		buf := make([]byte, 70000)
		up.SetReadDeadline(time.Now().Add(5 * time.Second))
		var from *net.UDPAddr
		for _, want := range [][]byte{[]byte("abc"), []byte("defgh"), big} {
			n, addr, err := up.ReadFromUDP(buf)
			must(t, err)
			from = addr
			if !bytes.Equal(buf[:n], want) {
				t.Fatalf("datagram of %d bytes, want %d", n, len(want))
			}
		}
		_, err = up.WriteToUDP([]byte("xyz"), from)
		must(t, err)
		_, err = up.WriteToUDP([]byte("12"), from)
		must(t, err)
		got := make([]byte, 9)
		_, err = io.ReadFull(br, got)
		must(t, err)
		if string(got) != "\x00\x03xyz\x00\x0212" {
			t.Fatalf("framed downstream %q", got)
		}
		// A zero-length frame fails closed.
		_, err = c.Write([]byte{0, 0})
		must(t, err)
		expectClosed(t, c, br)
		expectNoDatagram(t, up)
	})
	t.Run("truncated large frame", func(t *testing.T) {
		c, br := openTunnel(t, addr, "port=47999&transport=udp", nil)
		_, err := c.Write(append([]byte{0xff, 0xdc}, bytes.Repeat([]byte{1}, 100)...)) // 65500 is allowed.
		must(t, err)
		_, err = c.Write([]byte{0xff, 0xe4}) // Still payload inside the declared frame, not another header.
		must(t, err)
		must(t, c.CloseWrite()) // EOF establishes truncation; an open stream may legally send more.
		expectClosed(t, c, br)
		expectNoDatagram(t, up)
	})
	for _, hdr := range [][]byte{{0xff, 0xe4}, {0xff, 0xff}} {
		t.Run(fmt.Sprintf("length above udp maximum %d", binary.BigEndian.Uint16(hdr)), func(t *testing.T) {
			c, br := openTunnel(t, addr, "port=47999&transport=udp", hdr)
			expectClosed(t, c, br)
			expectNoDatagram(t, up)
		})
	}
	t.Run("truncated frame", func(t *testing.T) {
		c, br := openTunnel(t, addr, "transport=udp&port=47999", append([]byte{0, 10}, "abc"...))
		must(t, c.CloseWrite())
		expectClosed(t, c, br)
		expectNoDatagram(t, up)
	})
}

func TestUpgradeAndConnectionCapacity(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	must(t, err)
	defer ln.Close()
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func() { io.Copy(io.Discard, c); c.Close() }()
		}
	}()
	cfg := testConfig(t)
	cfg.maxUpgrades = 1
	cfg.maxActive = 3
	cfg.tunnelTarget = func(string, string) string { return ln.Addr().String() }
	addr := startGateway(t, cfg)

	first, _ := openTunnel(t, addr, "port=47984&transport=tcp", nil)
	resp, _ := exchange(t, addr, tunnelRequest("port=47984&transport=tcp"))
	expectStatus(t, resp, 429)

	first.Close()
	deadline := time.Now().Add(3 * time.Second)
	for {
		resp, _ := exchange(t, addr, tunnelRequest("port=47984&transport=tcp"))
		if resp.StatusCode == 101 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("tunnel slot not released: %d", resp.StatusCode)
		}
		time.Sleep(50 * time.Millisecond)
	}

	// Fill every connection slot with idle connections; the next is refused.
	var idle []net.Conn
	defer func() {
		for _, c := range idle {
			c.Close()
		}
	}()
	time.Sleep(100 * time.Millisecond) // let the last tunnel's slot drain
	for i := 0; i < 3; i++ {
		c, err := net.Dial("tcp", addr)
		must(t, err)
		idle = append(idle, c)
	}
	time.Sleep(100 * time.Millisecond)
	resp, body := exchange(t, addr, get("/vnc.html"))
	expectStatus(t, resp, 503)
	if !strings.Contains(body, "service unavailable") {
		t.Fatalf("busy body %q", body)
	}
}

// Sunshine pairing.

type sunshineFixture struct {
	list       string
	listStatus int
	reply      string
	redirect   bool
	gets       atomic.Int32
	posts      atomic.Int32
	other      atomic.Int32
	posted     chan []byte
}

func startSunshine(t *testing.T, cfg *config, s *sunshineFixture) {
	t.Helper()
	s.posted = make(chan []byte, 4)
	srv := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/pin" {
			s.other.Add(1)
			http.NotFound(w, r)
			return
		}
		user, pass, ok := r.BasicAuth()
		if !ok || user != "owner" || pass != testCredential {
			w.WriteHeader(http.StatusUnauthorized)
			io.WriteString(w, "<html>login "+pass+"</html>")
			return
		}
		switch r.Method {
		case http.MethodGet:
			s.gets.Add(1)
			if s.redirect {
				w.Header().Set("Location", "/elsewhere")
				w.WriteHeader(http.StatusFound)
				return
			}
			if s.listStatus != 0 {
				w.WriteHeader(s.listStatus)
			}
			io.WriteString(w, s.list)
		case http.MethodPost:
			s.posts.Add(1)
			b, _ := io.ReadAll(r.Body)
			s.posted <- b
			io.WriteString(w, s.reply)
		}
	}))
	t.Cleanup(srv.Close)
	cfg.sunshineAddr = srv.Listener.Addr().String()
}

func writeCredential(t *testing.T, cfg *config, content string) (dir, path string) {
	t.Helper()
	base := tempBase(t)
	dir = filepath.Join(base, "native-ui")
	must(t, os.Mkdir(dir, 0o700))
	must(t, os.Chmod(dir, 0o700))
	path = filepath.Join(dir, "sunshine-password")
	must(t, os.WriteFile(path, []byte(content), 0o600))
	must(t, os.Chmod(path, 0o600))
	var dst, fst syscall.Stat_t
	must(t, syscall.Lstat(dir, &dst))
	must(t, syscall.Lstat(path, &fst))
	if dst.Gid != fst.Gid {
		t.Skip("file and directory groups differ on this filesystem")
	}
	cfg.credPath = path
	cfg.credUID = fst.Uid
	cfg.credGID = fst.Gid
	return dir, path
}

func pairRequest(body string, extra ...string) string {
	return post("/api/moonlight/pair", "application/json", body, append([]string{"X-KVM-Box-Request: 1\r\n"}, extra...)...)
}

func TestPairDeadlineAndCancellation(t *testing.T) {
	for _, cancelClient := range []bool{false, true} {
		t.Run(fmt.Sprint("clientCancel=", cancelClient), func(t *testing.T) {
			cfg := testConfig(t)
			entered := make(chan struct{})
			closed := make(chan struct{})
			srv := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				close(entered)
				<-r.Context().Done()
				close(closed)
			}))
			defer srv.Close()
			cfg.sunshineAddr = srv.Listener.Addr().String()
			g := newGateway(cfg)
			defer g.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
			defer cancel()
			result := make(chan int, 1)
			go func() {
				_, code := g.pairSunshine(ctx, []byte(testCredential), "1234", "Fixture")
				result <- code
			}()
			select {
			case <-entered:
			case <-time.After(2 * time.Second):
				t.Fatal("Sunshine request did not start")
			}
			if cancelClient {
				cancel()
			}
			select {
			case code := <-result:
				if code != http.StatusServiceUnavailable {
					t.Fatalf("status %d", code)
				}
			case <-time.After(2 * time.Second):
				t.Fatal("pairing outlived context")
			}
			select {
			case <-closed:
			case <-time.After(2 * time.Second):
				t.Fatal("Sunshine connection was retained")
			}
		})
	}
}

func TestPairSuccess(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential+"\n")
	s := &sunshineFixture{list: `{"pairings":[{"id":"abc-1","name":"moonlight"}],"status":true}`, reply: `{"status":true}`}
	startSunshine(t, &cfg, s)
	addr := startGateway(t, cfg)
	resp, body := exchange(t, addr, pairRequest(`{"pin":"0123","name":"Living room"}`))
	expectStatus(t, resp, 200)
	expectSecurityHeaders(t, resp)
	if body != `{"status":true}` || resp.Header.Get("Content-Type") != "application/json" {
		t.Fatalf("body %q", body)
	}
	var sent map[string]string
	must(t, json.Unmarshal(<-s.posted, &sent))
	want := map[string]string{"pairing_id": "abc-1", "pin": "0123", "name": "Living room"}
	if fmt.Sprint(sent) != fmt.Sprint(want) {
		t.Fatalf("Sunshine received %v", sent)
	}
}

func TestPairDefaultNameAndStringStatus(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: `{"pairings":[{"id":"7"}]}`, reply: `{"status":"false"}`}
	startSunshine(t, &cfg, s)
	addr := startGateway(t, cfg)
	resp, body := exchange(t, addr, pairRequest(`{"pin":"9999"}`))
	expectStatus(t, resp, 200)
	if body != `{"status":false}` {
		t.Fatalf("body %q", body)
	}
	var sent map[string]string
	must(t, json.Unmarshal(<-s.posted, &sent))
	if sent["name"] != "KVM Box owner" || sent["pairing_id"] != "7" {
		t.Fatalf("Sunshine received %v", sent)
	}
}

func TestPairRejectsInputWithoutContactingSunshine(t *testing.T) {
	cfg := testConfig(t)
	writeCredential(t, &cfg, testCredential)
	s := &sunshineFixture{list: `{"pairings":[{"id":"abc"}]}`, reply: `{"status":true}`}
	startSunshine(t, &cfg, s)
	addr := startGateway(t, cfg)
	const target = "/api/moonlight/pair"
	cases := []struct {
		name, raw string
		code      int
	}{
		{"no request header", post(target, "application/json", `{"pin":"1234"}`), 403},
		{"request header 2", post(target, "application/json", `{"pin":"1234"}`, "X-KVM-Box-Request: 2\r\n"), 403},
		{"duplicate request header", pairRequest(`{"pin":"1234"}`, "X-KVM-Box-Request: 1\r\n"), 400},
		{"letter in pin", pairRequest(`{"pin":"12a4"}`), 400},
		{"short pin", pairRequest(`{"pin":"123"}`), 400},
		{"long pin", pairRequest(`{"pin":"12345"}`), 400},
		{"non-ascii digits", pairRequest(`{"pin":"١٢٣٤"}`), 400},
		{"numeric pin", pairRequest(`{"pin":1234}`), 400},
		{"missing pin", pairRequest(`{"name":"x"}`), 400},
		{"unknown field", pairRequest(`{"pin":"1234","pairing_id":"other"}`), 400},
		{"duplicate pin", pairRequest(`{"pin":"1234","pin":"5678"}`), 400},
		{"control in name", pairRequest(`{"pin":"1234","name":"a\u0007b"}`), 400},
		{"newline in name", pairRequest(`{"pin":"1234","name":"a\nb"}`), 400},
		{"long name", pairRequest(`{"pin":"1234","name":"` + strings.Repeat("n", 81) + `"}`), 400},
		{"numeric name", pairRequest(`{"pin":"1234","name":5}`), 400},
		{"nested name", pairRequest(`{"pin":"1234","name":{"a":1}}`), 400},
		{"trailing data", pairRequest(`{"pin":"1234"}x`), 400},
		{"array", pairRequest(`["1234"]`), 400},
		{"text/plain", post(target, "text/plain", `{"pin":"1234"}`, "X-KVM-Box-Request: 1\r\n"), 415},
		{"too large", pairRequest(`{"pin":"1234","name":"` + strings.Repeat("n", 1024) + `"}`), 413},
		{"query", post(target+"?pin=1234", "application/json", `{"pin":"1234"}`, "X-KVM-Box-Request: 1\r\n"), 400},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			resp, _ := exchange(t, addr, tc.raw)
			expectStatus(t, resp, tc.code)
		})
	}
	if s.gets.Load()+s.posts.Load()+s.other.Load() != 0 {
		t.Fatal("Sunshine contacted for rejected input")
	}
	// The 80-character limit counts characters, not bytes.
	resp, _ := exchange(t, addr, pairRequest(`{"pin":"1234","name":"`+strings.Repeat("é", 80)+`"}`))
	expectStatus(t, resp, 200)
}

func TestPairPendingAndUpstreamSanitizing(t *testing.T) {
	cases := []struct {
		name string
		s    sunshineFixture
		code int
		body string
	}{
		{"no pending pairing", sunshineFixture{list: `{"pairings":[]}`}, 409, pairCountBody},
		{"two pending pairings", sunshineFixture{list: `{"pairings":[{"id":"a"},{"id":"b"}]}`}, 409, pairCountBody},
		{"missing pairings", sunshineFixture{list: `{"status":true}`}, 502, `{"error":"bad gateway"}`},
		{"numeric id", sunshineFixture{list: `{"pairings":[{"id":7}]}`}, 502, `{"error":"bad gateway"}`},
		{"id with space", sunshineFixture{list: `{"pairings":[{"id":"a b"}]}`}, 502, `{"error":"bad gateway"}`},
		{"oversized list", sunshineFixture{list: `{"pairings":[{"id":"a","pad":"` + strings.Repeat("p", 5000) + `"}]}`}, 502, `{"error":"bad gateway"}`},
		{"html list", sunshineFixture{list: `<html>sunshine</html>`}, 502, `{"error":"bad gateway"}`},
		{"list error status", sunshineFixture{list: `<html>oops</html>`, listStatus: 500}, 502, `{"error":"bad gateway"}`},
		{"redirect", sunshineFixture{redirect: true}, 502, `{"error":"bad gateway"}`},
		{"html reply", sunshineFixture{list: `{"pairings":[{"id":"a"}]}`, reply: `<html><script>x</script></html>`}, 502, `{"error":"bad gateway"}`},
		{"reply without status", sunshineFixture{list: `{"pairings":[{"id":"a"}]}`, reply: `{"ok":true}`}, 502, `{"error":"bad gateway"}`},
		{"reply status number", sunshineFixture{list: `{"pairings":[{"id":"a"}]}`, reply: `{"status":1}`}, 502, `{"error":"bad gateway"}`},
	}
	for i := range cases {
		tc := &cases[i]
		t.Run(tc.name, func(t *testing.T) {
			cfg := testConfig(t)
			writeCredential(t, &cfg, testCredential)
			startSunshine(t, &cfg, &tc.s)
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
			expectStatus(t, resp, tc.code)
			if body != tc.body {
				t.Fatalf("body %q", body)
			}
			if tc.code == 409 && tc.s.posts.Load() != 0 {
				t.Fatal("PIN submitted without exactly one pending pairing")
			}
			if tc.s.other.Load() != 0 {
				t.Fatal("redirect followed")
			}
		})
	}
	t.Run("credential rejected by Sunshine", func(t *testing.T) {
		cfg := testConfig(t)
		writeCredential(t, &cfg, "different-credential")
		s := &sunshineFixture{list: `{"pairings":[{"id":"a"}]}`}
		startSunshine(t, &cfg, s)
		addr := startGateway(t, cfg)
		resp, body := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
		expectStatus(t, resp, 503)
		if strings.Contains(body, "different") || strings.Contains(body, "html") {
			t.Fatalf("credential or upstream body reflected: %q", body)
		}
	})
	t.Run("sunshine down", func(t *testing.T) {
		cfg := testConfig(t)
		writeCredential(t, &cfg, testCredential)
		addr := startGateway(t, cfg)
		resp, _ := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
		expectStatus(t, resp, 503)
	})
}

func TestPairCredentialFileChecks(t *testing.T) {
	cases := []struct {
		name    string
		content string
		mutate  func(t *testing.T, cfg *config, dir, path string)
	}{
		{"file mode 0644", testCredential, func(t *testing.T, cfg *config, dir, path string) { must(t, os.Chmod(path, 0o644)) }},
		{"file mode 0400", testCredential, func(t *testing.T, cfg *config, dir, path string) { must(t, os.Chmod(path, 0o400)) }},
		{"dir mode 0750", testCredential, func(t *testing.T, cfg *config, dir, path string) { must(t, os.Chmod(dir, 0o750)) }},
		{"foreign owner", testCredential, func(t *testing.T, cfg *config, dir, path string) { cfg.credUID++ }},
		{"foreign group", testCredential, func(t *testing.T, cfg *config, dir, path string) { cfg.credGID++ }},
		{"symlinked file", testCredential, func(t *testing.T, cfg *config, dir, path string) {
			real := filepath.Join(dir, "real")
			must(t, os.Rename(path, real))
			must(t, os.Symlink(real, path))
		}},
		{"symlinked directory", testCredential, func(t *testing.T, cfg *config, dir, path string) {
			link := filepath.Join(filepath.Dir(dir), "ui-link")
			must(t, os.Symlink(dir, link))
			cfg.credPath = filepath.Join(link, "sunshine-password")
		}},
		{"missing", testCredential, func(t *testing.T, cfg *config, dir, path string) { must(t, os.Remove(path)) }},
		{"empty", "", nil},
		{"only newline", "\n", nil},
		{"leading space", " " + testCredential, nil},
		{"trailing space", testCredential + " ", nil},
		{"internal newline", "abc\ndef", nil},
		{"two newlines", testCredential + "\n\n", nil},
		{"crlf", testCredential + "\r\n", nil},
		{"non-ascii", "pässword", nil},
		{"too long", strings.Repeat("x", 129), nil},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := testConfig(t)
			dir, path := writeCredential(t, &cfg, tc.content)
			if tc.mutate != nil {
				tc.mutate(t, &cfg, dir, path)
			}
			s := &sunshineFixture{list: `{"pairings":[{"id":"a"}]}`, reply: `{"status":true}`}
			startSunshine(t, &cfg, s)
			addr := startGateway(t, cfg)
			resp, body := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
			expectStatus(t, resp, 503)
			if body != `{"error":"service unavailable"}` {
				t.Fatalf("body %q", body)
			}
			if s.gets.Load()+s.posts.Load() != 0 {
				t.Fatal("Sunshine contacted with a refused credential")
			}
		})
	}
	t.Run("128 bytes accepted", func(t *testing.T) {
		cfg := testConfig(t)
		cred := strings.Repeat("x", 128)
		writeCredential(t, &cfg, cred)
		var seen atomic.Value
		srv := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			_, pass, _ := r.BasicAuth()
			seen.Store(pass)
			io.WriteString(w, `{"pairings":[]}`)
		}))
		defer srv.Close()
		cfg.sunshineAddr = srv.Listener.Addr().String()
		addr := startGateway(t, cfg)
		resp, _ := exchange(t, addr, pairRequest(`{"pin":"1234"}`))
		expectStatus(t, resp, 409)
		if seen.Load() != cred {
			t.Fatal("credential altered before use")
		}
	})
}
