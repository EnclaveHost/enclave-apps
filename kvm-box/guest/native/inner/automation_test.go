//go:build linux

package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func startAutomation(t *testing.T, cfg *config, handler func(net.Conn)) string {
	t.Helper()
	dir := filepath.Join(tempBase(t), "automation")
	must(t, os.Mkdir(dir, 0750))
	must(t, os.Chmod(dir, 0750))
	path := filepath.Join(dir, "desktop.sock")
	ln, e := net.Listen("unix", path)
	must(t, e)
	must(t, os.Chmod(path, 0660))
	t.Cleanup(func() { ln.Close() })
	cfg.automationSocket = path
	cfg.automationUID = uint32(os.Getuid())
	cfg.automationGID = uint32(os.Getgid())
	cfg.automationPeerUID = uint32(os.Getuid())
	go func() {
		for {
			c, e := ln.Accept()
			if e != nil {
				return
			}
			go func() { defer c.Close(); c.SetDeadline(time.Now().Add(2 * time.Second)); handler(c) }()
		}
	}()
	return path
}
func automationReply(body, headers string) string {
	return fmt.Sprintf("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\n%sConnection: close\r\n\r\n%s", len(body), headers, body)
}
func fixedAutomationReply(reply string) func(net.Conn) {
	return func(c net.Conn) {
		r, e := http.ReadRequest(bufio.NewReader(c))
		if e != nil {
			return
		}
		io.Copy(io.Discard, r.Body)
		r.Body.Close()
		io.WriteString(c, reply)
	}
}

func TestAutomationForwardingAndUntrustedHeaders(t *testing.T) {
	cfg := testConfig(t)
	seen := make(chan *http.Request, 1)
	startAutomation(t, &cfg, func(c net.Conn) {
		r, e := http.ReadRequest(bufio.NewReader(c))
		if e != nil {
			return
		}
		b, _ := io.ReadAll(r.Body)
		r.Body.Close()
		if string(b) != `{"cmd":"printf native"}` {
			return
		}
		seen <- r
		io.WriteString(c, automationReply(`{"ok":true,"output":"</script><script>bad</script>","exitCode":0,"truncated":false,"ms":1}`, "Set-Cookie: desktop=untrusted\r\nLocation: https://untrusted.invalid/\r\n"))
	})
	addr := startGateway(t, cfg)
	body := `{"cmd":"printf native"}`
	resp, got := exchange(t, addr, fmt.Sprintf("POST /exec HTTP/1.1\r\n%sContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", hostLine, len(body), body))
	if resp.StatusCode != 200 || !strings.Contains(got, `"ok":true`) {
		t.Fatalf("forward failed: %d %s", resp.StatusCode, got)
	}
	if resp.Header.Get("Set-Cookie") != "" || resp.Header.Get("Location") != "" || strings.Contains(got, "<script>") {
		t.Fatal("desktop-controlled headers/HTML escaped the boundary")
	}
	select {
	case req := <-seen:
		if req.URL.Path != "/exec" || req.Host != "desktop" || req.Header.Get("Authorization") != "" || req.Header.Get("X-Api-Key") != "" {
			t.Fatal("incorrect upstream request")
		}
	case <-time.After(time.Second):
		t.Fatal("no upstream request")
	}
}

func TestAutomationRefusesSocketMetadataAndPeerBeforeSending(t *testing.T) {
	for _, kind := range []string{"peer", "directory", "socket", "symlink"} {
		t.Run(kind, func(t *testing.T) {
			cfg := testConfig(t)
			read := make(chan int, 1)
			path := startAutomation(t, &cfg, func(c net.Conn) { var b [1]byte; n, _ := c.Read(b[:]); read <- n })
			switch kind {
			case "peer":
				cfg.automationPeerUID++
			case "directory":
				must(t, os.Chmod(filepath.Dir(path), 0770))
			case "socket":
				must(t, os.Chmod(path, 0666))
			case "symlink":
				alias := path + "-alias"
				must(t, os.Symlink(path, alias))
				cfg.automationSocket = alias
			}
			g := newGateway(cfg)
			defer g.Close()
			_, _, e := g.automationExchange(context.Background(), "/exec", []byte(`{"cmd":"true"}`), time.Now().Add(time.Second))
			if e == nil {
				t.Fatal("unsafe endpoint admitted")
			}
			if kind == "peer" {
				select {
				case n := <-read:
					if n != 0 {
						t.Fatal("request leaked before peer check")
					}
				case <-time.After(time.Second):
					t.Fatal("peer fixture did not close")
				}
			}
		})
	}
}

func TestAutomationRejectsMalformedRepliesWithoutDraining(t *testing.T) {
	for name, reply := range map[string]string{
		"html":             "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 13\r\n\r\n<script></script>",
		"duplicate json":   automationReply(`{"ok":true,"ok":false}`, ""),
		"missing status":   automationReply(`{"output":"untrusted"}`, ""),
		"truncated":        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 99\r\n\r\n{}",
		"oversized body":   fmt.Sprintf("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n", maxAutomationResponse+1),
		"oversized header": "HTTP/1.1 200 OK\r\nX-Fill: " + strings.Repeat("x", maxHeaderBytes+1),
		"redirect":         "HTTP/1.1 302 Found\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
		"chunked":          "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{\"ok\":true}\r\n0\r\n\r\n",
	} {
		t.Run(name, func(t *testing.T) {
			cfg := testConfig(t)
			startAutomation(t, &cfg, fixedAutomationReply(reply))
			g := newGateway(cfg)
			defer g.Close()
			_, _, e := g.automationExchange(context.Background(), "/computer", []byte(`{}`), time.Now().Add(time.Second))
			if e == nil {
				t.Fatal("malformed response accepted")
			}
		})
	}
	// A huge declared body without any actual bytes must be rejected before
	// the deadline, not drained by http.Response.Body.Close.
	cfg := testConfig(t)
	sent := make(chan struct{})
	startAutomation(t, &cfg, func(c net.Conn) {
		r, e := http.ReadRequest(bufio.NewReader(c))
		if e != nil {
			return
		}
		io.Copy(io.Discard, r.Body)
		io.WriteString(c, fmt.Sprintf("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n", maxAutomationResponse+1))
		close(sent)
		var b [1]byte
		c.Read(b[:])
	})
	g := newGateway(cfg)
	defer g.Close()
	start := time.Now()
	_, _, e := g.automationExchange(context.Background(), "/exec", []byte(`{}`), time.Now().Add(2*time.Second))
	if e == nil || time.Since(start) > 500*time.Millisecond {
		t.Fatal("oversized response was drained")
	}
	<-sent
}

func TestAutomationDeadlineAndInvalidBody(t *testing.T) {
	cfg := testConfig(t)
	startAutomation(t, &cfg, func(c net.Conn) { var b [4096]byte; c.Read(b[:]); time.Sleep(300 * time.Millisecond) })
	g := newGateway(cfg)
	defer g.Close()
	start := time.Now()
	_, _, e := g.automationExchange(context.Background(), "/exec", []byte(`{}`), time.Now().Add(30*time.Millisecond))
	if e == nil || time.Since(start) > 200*time.Millisecond {
		t.Fatal("deadline not honored")
	}
	addr := startGateway(t, cfg)
	for _, body := range []string{`{"cmd":"true","cmd":"false"}`, `{} {}`, `[]`} {
		r, _ := exchange(t, addr, fmt.Sprintf("POST /exec HTTP/1.1\r\n%sContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", hostLine, len(body), body))
		if r.StatusCode != 400 {
			t.Fatal("ambiguous input accepted", r.StatusCode)
		}
	}
}
