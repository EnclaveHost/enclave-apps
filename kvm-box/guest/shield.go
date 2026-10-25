package main

import (
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"time"
)

// Called only after owner authentication and the same-origin check. The guest
// service owns backend selection, weight approval and input validation.
func shieldCompute(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/api/shield/boundary" && r.Method == "GET" {
		data, err := os.ReadFile("/run/shield/boundary.json")
		if err != nil {
			http.Error(w, "Boundary checks unavailable", http.StatusServiceUnavailable)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Cache-Control", "no-store")
		w.Write(data)
		return
	}
	var request []byte
	if r.URL.Path == "/api/shield/status" && r.Method == "GET" {
		request = []byte(`{"op":"status"}`)
	} else if r.URL.Path == "/api/shield/compute" && r.Method == "POST" {
		r.Body = http.MaxBytesReader(w, r.Body, 8<<20)
		data, err := io.ReadAll(r.Body)
		if err != nil || !json.Valid(data) {
			http.Error(w, "Invalid compute request", http.StatusBadRequest)
			return
		}
		request = data
	} else {
		http.Error(w, "Unsupported compute route or method", http.StatusMethodNotAllowed)
		return
	}
	conn, err := net.DialTimeout("unix", "/run/shield/compute.sock", 3*time.Second)
	if err != nil {
		http.Error(w, "Native Shield service unavailable", http.StatusServiceUnavailable)
		return
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(21 * time.Minute))
	header := make([]byte, 4)
	binary.BigEndian.PutUint32(header, uint32(len(request)))
	if _, err = conn.Write(append(header, request...)); err != nil {
		http.Error(w, "Compute request failed", http.StatusBadGateway)
		return
	}
	if _, err = io.ReadFull(conn, header); err != nil {
		http.Error(w, "Compute response failed", http.StatusBadGateway)
		return
	}
	size := binary.BigEndian.Uint32(header)
	if size == 0 || size > 16<<20 {
		http.Error(w, "Invalid compute response", http.StatusBadGateway)
		return
	}
	data := make([]byte, size)
	if _, err = io.ReadFull(conn, data); err != nil || !json.Valid(data) {
		http.Error(w, "Invalid compute response", http.StatusBadGateway)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Write(data)
}
