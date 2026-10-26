package main

import (
	"crypto/sha256"
	"crypto/subtle"
	"enclave.host/isolation/m2/vsock"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"runtime"
	"strings"
	"time"
)

func main() {
	if len(os.Args) != 2 {
		log.Fatal("usage: helper bridge|gateway")
	}
	if os.Args[1] == "bridge" {
		ln, e := net.Listen("tcp", "127.0.0.1:19080")
		if e != nil {
			log.Fatal(e)
		}
		for {
			c, e := ln.Accept()
			if e != nil {
				log.Fatal(e)
			}
			go func() {
				defer c.Close()
				up, e := vsock.Dial(2, 19080)
				if e != nil {
					return
				}
				defer up.Close()
				c.SetDeadline(time.Now().Add(10 * time.Second))
				up.SetDeadline(time.Now().Add(10 * time.Second))
				done := make(chan struct{}, 2)
				go func() { io.Copy(up, c); done <- struct{}{} }()
				go func() { io.Copy(c, up); done <- struct{}{} }()
				<-done
			}()
		}
	}
	if os.Args[1] != "gateway" {
		log.Fatal("unknown mode")
	}
	b, e := os.ReadFile("/access.sha256")
	if e != nil {
		log.Fatal(e)
	}
	wanted, e := hex.DecodeString(strings.TrimSpace(string(b)))
	if e != nil || len(wanted) != 32 {
		log.Fatal("invalid access hash")
	}
	target, _ := url.Parse("http://127.0.0.1:6080")
	proxy := httputil.NewSingleHostReverseProxy(target)
	proxy.ModifyResponse = func(r *http.Response) error {
		r.Header.Set("Cache-Control", "no-store")
		r.Header.Set("X-Content-Type-Options", "nosniff")
		return nil
	}
	computeTarget, _ := url.Parse("http://127.0.0.1:8765")
	computeProxy := httputil.NewSingleHostReverseProxy(computeTarget)
	computeDirector := computeProxy.Director
	computeProxy.Director = func(r *http.Request) {
		computeDirector(r)
		r.Host = "127.0.0.1:8765"
		r.Header.Del("Authorization")
		if token := r.Header.Get("X-Compute-Authorization"); token != "" {
			r.Header.Set("Authorization", token)
		}
		r.Header.Del("X-Compute-Authorization")
		r.Header.Del("Origin")
		r.URL.Path = strings.TrimPrefix(r.URL.Path, "/compute")
		if r.URL.Path == "" {
			r.URL.Path = "/"
		}
	}
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		user, pass, ok := r.BasicAuth()
		sum := sha256.Sum256([]byte(pass))
		if !ok || user != "owner" || subtle.ConstantTimeCompare(sum[:], wanted) != 1 {
			w.Header().Set("WWW-Authenticate", `Basic realm="KVM Box"`)
			http.Error(w, "Sign in as owner.", 401)
			return
		}
		if origin := r.Header.Get("Origin"); origin != "" && origin != "https://"+r.Host && origin != "http://"+r.Host {
			http.Error(w, "Origin refused", 403)
			return
		}
		if r.URL.Path == "/compute/" || strings.HasPrefix(r.URL.Path, "/compute/") {
			computeProxy.ServeHTTP(w, r)
			return
		}
		if r.URL.Path == "/api/moonlight/tunnel" {
			moonlightTunnel(w, r)
			return
		}
		if strings.HasPrefix(r.URL.Path, "/api/shield/") {
			shieldCompute(w, r)
			return
		}
		if r.URL.Path == "/api/moonlight/pair" {
			moonlightPair(w, r)
			return
		}
		if r.URL.Path == "/api/diagnostics" {
			if r.Method != "GET" {
				http.Error(w, "GET only", 405)
				return
			}
			logs := map[string]string{}
			for _, name := range []string{"xvfb", "session", "vnc", "websockify", "sunshine", "compute", "xorg", "xdoom"} {
				path := "/tmp/" + name + ".log"
				if name == "xorg" {
					path = "/var/log/Xorg.0.log"
				}
				data, _ := os.ReadFile(path)
				if len(data) > 8192 {
					data = data[len(data)-8192:]
				}
				logs[name] = string(data)
			}
			w.Header().Set("Content-Type", "application/json")
			json.NewEncoder(w).Encode(logs)
			return
		}
		if r.URL.Path == "/api/desktop" {
			if r.Method != "GET" {
				http.Error(w, "GET only", 405)
				return
			}
			firefox := false
			applications := map[string]int{}
			entries, _ := os.ReadDir("/proc")
			for _, p := range entries {
				v, _ := os.ReadFile("/proc/" + p.Name() + "/comm")
				name := strings.TrimSpace(string(v))
				if strings.Contains(name, "firefox") {
					firefox = true
				}
				for _, wanted := range []string{"firefox-esr", "fluxbox", "xdoom", "pacat", "sunshine"} {
					if name == wanted {
						applications[name]++
					}
				}
			}
			w.Header().Set("Content-Type", "application/json")
			json.NewEncoder(w).Encode(map[string]any{"architecture": runtime.GOARCH, "cpus": runtime.NumCPU(), "firefoxRunning": firefox, "applications": applications, "execution": "native-x86_64", "storage": "ephemeral"})
			return
		}
		r.Header.Del("Authorization")
		r.Header.Del("X-Forwarded-For")
		proxy.ServeHTTP(w, r)
	})
	fmt.Println("DESKTOP gateway listening on 8000")
	log.Fatal((&http.Server{Addr: "127.0.0.1:8000", Handler: handler, ReadHeaderTimeout: 5 * time.Second, MaxHeaderBytes: 16384, IdleTimeout: 30 * time.Second}).ListenAndServe())
}
