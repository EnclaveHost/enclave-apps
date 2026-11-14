//go:build linux

package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"io"
	"net/http"
	"os"
	"strings"
	"syscall"
)

// Sunshine keeps one record per completed pairing and refuses a client
// certificate that more than one record carries. Its admin API lists the
// records without their certificates, and its state file belongs to the
// desktop. So the inner gateway, which holds the admin credential, asks here
// which certificate each record carries: a digest per record, nothing else of
// the file. The path is fixed and only the inner gateway can connect.
const (
	sunshineState    = "/home/desktop/.config/sunshine/sunshine_state.json"
	maxStateBytes    = 4 << 20
	maxPairedClients = 512
	maxPairedUUID    = 64
)

var errPaired = errors.New("paired clients unavailable")

type pairedClient struct {
	UUID string `json:"uuid"`
	Cert string `json:"cert_sha256"`
}

func validPairedUUID(s string) bool {
	if len(s) < 1 || len(s) > maxPairedUUID {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F' || c == '-') {
			return false
		}
	}
	return true
}

// certDigest names a certificate by its DER bytes, so two PEM spellings of
// one certificate agree. Text that is not one PEM certificate is named by
// itself: equal text is still the same certificate to Sunshine.
func certDigest(text string) string {
	var sum [sha256.Size]byte
	block, rest := pem.Decode([]byte(text))
	if block != nil && block.Type == "CERTIFICATE" && len(bytes.TrimSpace(rest)) == 0 {
		sum = sha256.Sum256(block.Bytes)
	} else {
		sum = sha256.Sum256([]byte("text:" + strings.TrimSpace(text)))
	}
	return hex.EncodeToString(sum[:])
}

// parsePaired reads the records in stored order. Sunshine writes this file
// with a property-tree encoder: a list with no records is the empty string.
func parsePaired(b []byte) ([]pairedClient, error) {
	var state struct {
		Root *struct {
			Devices json.RawMessage `json:"named_devices"`
		} `json:"root"`
	}
	if err := json.Unmarshal(b, &state); err != nil {
		return nil, errPaired
	}
	out := []pairedClient{}
	if state.Root == nil {
		return out, nil
	}
	raw := bytes.TrimSpace(state.Root.Devices)
	if len(raw) == 0 || string(raw) == `""` || string(raw) == "null" {
		return out, nil
	}
	var devices []struct {
		UUID *string `json:"uuid"`
		Cert *string `json:"cert"`
	}
	if err := json.Unmarshal(raw, &devices); err != nil || len(devices) > maxPairedClients {
		return nil, errPaired
	}
	seen := make(map[string]bool, len(devices))
	for _, d := range devices {
		if d.UUID == nil || d.Cert == nil || !validPairedUUID(*d.UUID) || seen[*d.UUID] || strings.TrimSpace(*d.Cert) == "" {
			return nil, errPaired
		}
		seen[*d.UUID] = true
		out = append(out, pairedClient{UUID: *d.UUID, Cert: certDigest(*d.Cert)})
	}
	return out, nil
}

func readPaired(path string) ([]pairedClient, error) {
	f, err := os.OpenFile(path, os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, errPaired
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil || !st.Mode().IsRegular() || st.Size() > maxStateBytes {
		return nil, errPaired
	}
	b, err := io.ReadAll(io.LimitReader(f, maxStateBytes+1))
	if err != nil || len(b) > maxStateBytes {
		return nil, errPaired
	}
	return parsePaired(b)
}

func (s *service) servePaired(w http.ResponseWriter, r *http.Request) {
	select {
	case s.pairedSlots <- struct{}{}:
		defer func() { <-s.pairedSlots }()
	default:
		jsonReply(w, 429, map[string]any{"ok": false, "error": "paired clients busy"})
		return
	}
	// The request carries nothing: an empty object, so the path stays fixed.
	r.Body = http.MaxBytesReader(w, r.Body, 16)
	defer r.Body.Close()
	body, err := io.ReadAll(r.Body)
	if err != nil || string(bytes.TrimSpace(body)) != "{}" {
		jsonReply(w, 400, map[string]any{"ok": false, "error": "invalid paired request"})
		return
	}
	clients, err := readPaired(s.statePath)
	if err != nil {
		jsonReply(w, 503, map[string]any{"ok": false, "error": err.Error()})
		return
	}
	jsonReply(w, 200, map[string]any{"ok": true, "clients": clients})
}
