package main

import (
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

var tunnelSlots = make(chan struct{}, 32)
var moonlightTCP = map[int]bool{47984: true, 47989: true, 48010: true}
var moonlightUDP = map[int]bool{47998: true, 47999: true, 48000: true}

// Only fixed Sunshine ports are reachable. Authorization is checked by the gateway.
func moonlightTunnel(w http.ResponseWriter, r *http.Request) {
	port, _ := strconv.Atoi(r.URL.Query().Get("port"))
	transport := r.URL.Query().Get("transport")
	if r.Method != "GET" || r.Header.Get("Upgrade") != "kvm-box-tunnel" || (transport != "tcp" && transport != "udp") || (transport == "tcp" && !moonlightTCP[port]) || (transport == "udp" && !moonlightUDP[port]) {
		http.Error(w, "Invalid tunnel", 400)
		return
	}
	select {
	case tunnelSlots <- struct{}{}:
		defer func() { <-tunnelSlots }()
	default:
		http.Error(w, "Too many tunnels", 429)
		return
	}
	up, err := net.DialTimeout(transport, fmt.Sprintf("127.0.0.1:%d", port), 3*time.Second)
	if err != nil {
		http.Error(w, "Streaming server unavailable", 503)
		return
	}
	defer up.Close()
	client, rw, err := w.(http.Hijacker).Hijack()
	if err != nil {
		return
	}
	defer client.Close()
	rw.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n\r\n")
	if rw.Flush() != nil {
		return
	}
	done := make(chan struct{}, 2)
	if transport == "tcp" {
		go func() { io.Copy(up, rw); done <- struct{}{} }()
		go func() { io.Copy(client, up); done <- struct{}{} }()
	} else {
		go func() {
			defer func() { done <- struct{}{} }()
			var h [2]byte
			buf := make([]byte, 65535)
			for {
				client.SetReadDeadline(time.Now().Add(90 * time.Second))
				if _, e := io.ReadFull(rw, h[:]); e != nil {
					return
				}
				n := int(binary.BigEndian.Uint16(h[:]))
				if n == 0 {
					return
				}
				if _, e := io.ReadFull(rw, buf[:n]); e != nil {
					return
				}
				if _, e := up.Write(buf[:n]); e != nil {
					return
				}
			}
		}()
		go func() {
			defer func() { done <- struct{}{} }()
			buf := make([]byte, 65537)
			for {
				up.SetReadDeadline(time.Now().Add(90 * time.Second))
				n, e := up.Read(buf[2:])
				if e != nil {
					return
				}
				binary.BigEndian.PutUint16(buf[:2], uint16(n))
				client.SetWriteDeadline(time.Now().Add(10 * time.Second))
				if _, e = client.Write(buf[:n+2]); e != nil {
					return
				}
			}
		}()
	}
	<-done
}

// Sunshine's admin credential never leaves the guest. Only its PIN operation is exposed.
func moonlightPair(w http.ResponseWriter, r *http.Request) {
	if r.Method != "POST" || r.Header.Get("X-KVM-Box-Request") != "1" {
		http.Error(w, "Explicit POST required", 403)
		return
	}
	body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 1024))
	if err != nil {
		http.Error(w, "Invalid request", 400)
		return
	}
	pass, err := os.ReadFile("/run/sunshine-password")
	if err != nil {
		http.Error(w, "Sunshine not ready", 503)
		return
	}
	var input struct {
		Pin  string `json:"pin"`
		Name string `json:"name"`
	}
	if json.Unmarshal(body, &input) != nil || len(input.Pin) != 4 {
		http.Error(w, "Invalid PIN", 400)
		return
	}
	for _, digit := range input.Pin {
		if digit < '0' || digit > '9' {
			http.Error(w, "Invalid PIN", 400)
			return
		}
	}
	if input.Name == "" {
		input.Name = "KVM Box owner"
	}
	// Loopback inside this measured guest; Sunshine uses a locally generated cert.
	c := &http.Client{Timeout: 30 * time.Second, Transport: &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}}}
	pending, _ := http.NewRequest("GET", "https://127.0.0.1:47990/api/pin", nil)
	pending.SetBasicAuth("owner", strings.TrimSpace(string(pass)))
	list, e := c.Do(pending)
	if e != nil {
		http.Error(w, "Sunshine unavailable", 503)
		return
	}
	var requests struct {
		Pairings []struct {
			ID string `json:"id"`
		} `json:"pairings"`
	}
	e = json.NewDecoder(io.LimitReader(list.Body, 4096)).Decode(&requests)
	list.Body.Close()
	if e != nil || list.StatusCode != 200 {
		http.Error(w, "Cannot read pending pairing", 502)
		return
	}
	if len(requests.Pairings) != 1 {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(409)
		io.WriteString(w, `{"status":false,"error":"Start pairing in one Moonlight client, then submit its PIN."}`)
		return
	}
	payload, _ := json.Marshal(map[string]string{"pairing_id": requests.Pairings[0].ID, "pin": input.Pin, "name": input.Name})
	req, _ := http.NewRequest("POST", "https://127.0.0.1:47990/api/pin", strings.NewReader(string(payload)))
	req.SetBasicAuth("owner", strings.TrimSpace(string(pass)))
	req.Header.Set("Content-Type", "application/json")
	resp, err := c.Do(req)
	if err != nil {
		http.Error(w, "Sunshine unavailable", 503)
		return
	}
	defer resp.Body.Close()
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(resp.StatusCode)
	io.Copy(w, io.LimitReader(resp.Body, 4096))
}
